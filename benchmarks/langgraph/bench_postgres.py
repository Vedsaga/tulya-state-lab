#!/usr/bin/env python3
from __future__ import annotations
import argparse, importlib.metadata, json, os, struct, time, uuid
from collections.abc import Sequence
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Annotated, Any

import psycopg
from psycopg import sql
from psycopg.conninfo import conninfo_to_dict, make_conninfo
from typing_extensions import TypedDict

from langgraph.channels import DeltaChannel
from langgraph.checkpoint.base import BaseCheckpointSaver
from langgraph.checkpoint.postgres import PostgresSaver
from langgraph.graph import END, START, StateGraph
from langgraph.types import Overwrite

MASK64 = (1 << 64) - 1
DEFAULT_SEED = 0x5EED_1234_D15C_A11E
FNV_OFFSET = 0xCBF2_9CE4_8422_2325
FNV_PRIME = 0x0000_0100_0000_01B3
BASE_TEMPLATE = b'{"role":"assistant","tool":"state","status":"ok","content":"persistent branch data"}\\n'
EDIT_ALPHABET = b'{"role":"assistant","tool_call":true,"result":"ok"}\\n0123456789abcdef'
CROSS_TEMPLATES = (
    b'{"tool":"search","status":"ok","items":[{"kind":"result","score":0.91}],"trace":"shared-template-a"}\\n',
    b'{"tool":"shell","status":"ok","stdout":"build completed successfully","trace":"shared-template-b"}\\n',
    b'{"role":"assistant","content":"analysis checkpoint with repeated project context","trace":"shared-template-c"}\\n',
    b'{"artifact":"generated-code","language":"rust","status":"candidate","trace":"shared-template-d"}\\n',
)

@dataclass(frozen=True)
class Edit:
    start: int
    delete_len: int
    insert: bytes
    def output_len(self, input_len: int) -> int:
        end = self.start + self.delete_len
        if self.start < 0 or self.delete_len < 0 or self.start > input_len or end > input_len:
            raise ValueError("invalid edit")
        return input_len - self.delete_len + len(self.insert)

@dataclass(frozen=True)
class PlannedOp:
    parent: int
    edit: Edit
    read_start: int
    read_len: int

@dataclass
class Workload:
    kind: str
    base: bytes
    ops: list[PlannedOp]
    version_lengths: list[int]
    logical_version_bytes: int
    seed: int
    max_edit_bytes: int
    read_bytes: int

@dataclass
class LatencySummary:
    p50_us: float
    p95_us: float
    p99_us: float

@dataclass
class ModeResult:
    mode: str
    schema: str
    semantic_pass: bool
    semantic_error: str | None
    base_relation_bytes: int
    final_relation_bytes: int
    relation_growth_bytes: int
    growth_bytes_per_branch: float
    write: LatencySummary
    get_state_and_slice: LatencySummary
    reopen_get_state: LatencySummary
    checksum: str
    row_counts: dict[str, int]
    capabilities: dict[str, bool]
    branches: int
    logical_version_bytes: int

class Rng:
    def __init__(self, seed: int) -> None: self.x = seed & MASK64
    def next_u64(self) -> int:
        x = self.x
        x ^= (x << 13) & MASK64
        x ^= x >> 7
        x ^= (x << 17) & MASK64
        x &= MASK64
        self.x = x
        return x
    def usize(self, upper_exclusive: int) -> int:
        return 0 if upper_exclusive <= 0 else self.next_u64() % upper_exclusive

def rotl64(value: int, shift: int) -> int:
    value &= MASK64
    shift %= 64
    if shift == 0: return value
    return ((value << shift) | (value >> (64 - shift))) & MASK64

def structured_base(length: int, seed: int) -> bytes:
    out = bytearray(length)
    noise = Rng(seed ^ 0xA5A5_5A5A_C3C3_3C3C)
    for i in range(length):
        within_page = i % 4096
        if within_page < 64:
            page = i // 4096
            shift = within_page % 64
            out[i] = (rotl64(page, shift) ^ within_page) & 0xFF
        elif within_page < 3072:
            out[i] = BASE_TEMPLATE[i % len(BASE_TEMPLATE)]
        else:
            out[i] = noise.next_u64() & 0xFF
    return bytes(out)

def edit_payload(rng: Rng, length: int) -> bytes:
    out = bytearray(length)
    for i in range(length):
        jitter = rng.usize(len(EDIT_ALPHABET))
        out[i] = EDIT_ALPHABET[(i + jitter) % len(EDIT_ALPHABET)]
    return bytes(out)

def template_payload(template_id: int, length: int) -> bytes:
    template = CROSS_TEMPLATES[template_id % len(CROSS_TEMPLATES)]
    repeats, rem = divmod(max(length, 0), len(template))
    return template * repeats + template[:rem]

def planned_read(rng: Rng, child_len: int, read_bytes: int) -> tuple[int,int]:
    read_len = min(read_bytes, child_len)
    if read_len == 0: return 0,0
    return rng.usize(child_len-read_len+1), read_len

def generate_workload(kind: str, branches: int, base_bytes: int, max_edit_bytes: int, read_bytes: int, seed: int) -> Workload:
    base = structured_base(base_bytes, seed)
    rng = Rng(seed)
    version_lengths = [len(base)]
    logical = len(base)
    ops: list[PlannedOp] = []
    edit_scale = max(max_edit_bytes,1)
    def push(parent: int, edit: Edit) -> None:
        nonlocal logical
        child_len = edit.output_len(version_lengths[parent])
        rs, rl = planned_read(rng, child_len, read_bytes)
        ops.append(PlannedOp(parent, edit, rs, rl))
        version_lengths.append(child_len)
        logical += child_len
    for _ in range(branches):
        if kind == "small-edit":
            parent = rng.usize(len(version_lengths))
            plen = version_lengths[parent]
            start = rng.usize(plen+1)
            avail = plen-start
            mode = rng.usize(10)
            if mode <= 5:
                n = min(1+rng.usize(edit_scale), avail)
                delete_len, insert_len = n,n
            elif mode <= 7:
                delete_len, insert_len = 0,1+rng.usize(edit_scale)
            else:
                delete_len, insert_len = min(1+rng.usize(edit_scale), avail),0
            push(parent, Edit(start, delete_len, edit_payload(rng, insert_len)))
        elif kind == "append-heavy":
            parent = len(version_lengths)-1 if len(version_lengths)==1 or rng.usize(4)!=0 else rng.usize(len(version_lengths))
            plen = version_lengths[parent]
            ilen = 1+rng.usize(edit_scale)
            push(parent, Edit(plen,0,edit_payload(rng,ilen)))
        elif kind == "linear-append":
            parent = len(version_lengths)-1
            plen = version_lengths[parent]
            ilen = 1+rng.usize(edit_scale)
            push(parent, Edit(plen,0,edit_payload(rng,ilen)))
        elif kind == "cross-template":
            parent = rng.usize(len(version_lengths))
            plen = version_lengths[parent]
            payload_len = edit_scale
            tid = rng.usize(4)
            start = rng.usize(plen+1)
            avail = plen-start
            push(parent, Edit(start,min(payload_len,avail),template_payload(tid,payload_len)))
        elif kind == "large-rewrite":
            parent = rng.usize(len(version_lengths))
            plen = version_lengths[parent]
            rewrite_len = min(edit_scale,max(plen,1))
            start = rng.usize(plen-rewrite_len+1) if plen > rewrite_len else 0
            delete_len = min(rewrite_len, plen-start)
            push(parent, Edit(start,delete_len,edit_payload(rng,edit_scale)))
        else:
            raise ValueError(kind)
    return Workload(kind,base,ops,version_lengths,logical,seed,max_edit_bytes,read_bytes)

def apply_edit(parent: bytes, edit: Edit) -> bytes:
    end = edit.start+edit.delete_len
    return parent[:edit.start]+edit.insert+parent[end:]

def materialize_version(workload: Workload, index: int) -> bytes:
    chain=[]
    cur=index
    while cur:
        op=workload.ops[cur-1]
        chain.append(op)
        cur=op.parent
    state=workload.base
    for op in reversed(chain):
        state=apply_edit(state,op.edit)
    return state

def sample_indices(total: int, count: int) -> list[int]:
    count=max(2,min(count,total))
    return [total-1 if i+1==count else i*(total-1)//(count-1) for i in range(count)]

def encode_edit(edit: Edit) -> bytes:
    return struct.pack("<QQ",edit.start,edit.delete_len)+edit.insert

def apply_encoded_edit(current: bytes, encoded: bytes) -> bytes:
    if len(encoded)<16: raise ValueError("short encoded edit")
    start, delete_len = struct.unpack_from("<QQ",encoded,0)
    end=start+delete_len
    if start>len(current) or end>len(current): raise ValueError("encoded edit out of bounds")
    return current[:start]+encoded[16:]+current[end:]

def plain_reducer(current: bytes, update: bytes) -> bytes:
    return apply_encoded_edit(current,update)

def delta_reducer(current: bytes, writes: Sequence[bytes]) -> bytes:
    out=current
    for write in writes: out=apply_encoded_edit(out,write)
    return out

def build_graph(saver: PostgresSaver, mode: str, snapshot_frequency: int):
    if mode=="plain":
        class State(TypedDict):
            payload: Annotated[bytes, plain_reducer]
    elif mode=="delta":
        channel=DeltaChannel(delta_reducer,bytes,snapshot_frequency=snapshot_frequency)
        class State(TypedDict):
            payload: Annotated[bytes, channel]
    else: raise ValueError(mode)
    def bench_node(_state: State) -> dict[str,Any]: return {}
    b=StateGraph(State)
    b.add_node("bench",bench_node)
    b.add_edge(START,"bench")
    b.add_edge("bench",END)
    return b.compile(checkpointer=saver)

def fnv_update(value: int, data: bytes) -> int:
    for byte in data:
        value ^= byte
        value = (value*FNV_PRIME)&MASK64
    return value

def percentile_ns(values: list[int], pct: int) -> int:
    if not values: return 0
    s=sorted(values)
    return s[(len(s)-1)*pct//100]

def summarize(values: list[int]) -> LatencySummary:
    return LatencySummary(*(percentile_ns(values,p)/1000.0 for p in (50,95,99)))

def schema_conninfo(db_uri: str, schema: str) -> str:
    params=conninfo_to_dict(db_uri)
    existing=params.get("options","")
    opt=f"-csearch_path={schema}"
    params["options"]=f"{existing} {opt}".strip() if existing else opt
    return make_conninfo(**params)

def reset_schema(db_uri: str, schema: str) -> None:
    with psycopg.connect(db_uri,autocommit=True) as conn:
        conn.execute(sql.SQL("DROP SCHEMA IF EXISTS {} CASCADE").format(sql.Identifier(schema)))
        conn.execute(sql.SQL("CREATE SCHEMA {}").format(sql.Identifier(schema)))

def schema_relation_bytes(db_uri: str, schema: str) -> int:
    q="""SELECT COALESCE(SUM(pg_total_relation_size(c.oid)),0)
    FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname=%s AND c.relkind IN ('r','p','m')"""
    with psycopg.connect(db_uri,autocommit=True) as conn:
        return int(conn.execute(q,(schema,)).fetchone()[0])

def schema_row_counts(db_uri: str, schema: str) -> dict[str,int]:
    tables=("checkpoint_migrations","checkpoints","checkpoint_blobs","checkpoint_writes")
    out={}
    with psycopg.connect(db_uri,autocommit=True) as conn:
        for table in tables:
            exists=conn.execute("SELECT to_regclass(%s)",(f"{schema}.{table}",)).fetchone()[0]
            if exists is None:
                out[table]=0
            else:
                q=sql.SQL("SELECT COUNT(*) FROM {}.{}").format(sql.Identifier(schema),sql.Identifier(table))
                out[table]=int(conn.execute(q).fetchone()[0])
    return out

def saver_capabilities(saver: PostgresSaver) -> dict[str,bool]:
    out={}
    st=type(saver)
    for name in ("copy_thread","prune","get_delta_channel_history"):
        out[name]=getattr(st,name,None) is not getattr(BaseCheckpointSaver,name,None)
    return out

def verify_samples(graph, configs: list[dict[str,Any]], workload: Workload, count: int) -> None:
    for index in sample_indices(len(configs),count):
        actual=graph.get_state(configs[index]).values.get("payload",b"")
        expected=materialize_version(workload,index)
        if actual!=expected:
            raise AssertionError(f"semantic mismatch at version {index}: expected_len={len(expected)} actual_len={len(actual)}")

def run_mode(*,db_uri: str,schema: str,mode: str,workload: Workload,snapshot_frequency: int,verify_samples_count: int) -> ModeResult:
    reset_schema(db_uri,schema)
    scoped=schema_conninfo(db_uri,schema)
    tid=f"tulya-{mode}-{uuid.uuid4()}"
    writes=[]; reads=[]; checksum=FNV_OFFSET; configs=[]; capabilities={}
    semantic_pass=False; semantic_error=None; base_relation_bytes=0; reopen=[]
    try:
        with PostgresSaver.from_conn_string(scoped) as saver:
            saver.setup()
            capabilities=saver_capabilities(saver)
            graph=build_graph(saver,mode,snapshot_frequency)
            root={"configurable":{"thread_id":tid}}
            base_cfg=graph.update_state(root,{"payload":Overwrite(workload.base)},as_node="bench")
            configs.append(base_cfg)
            base_relation_bytes=schema_relation_bytes(db_uri,schema)
            for child_index,op in enumerate(workload.ops,start=1):
                parent_cfg=configs[op.parent]
                started=time.perf_counter_ns()
                child_cfg=graph.update_state(parent_cfg,{"payload":encode_edit(op.edit)},as_node="bench")
                writes.append(time.perf_counter_ns()-started)
                configs.append(child_cfg)
                started=time.perf_counter_ns()
                payload=graph.get_state(child_cfg).values.get("payload",b"")
                piece=payload[op.read_start:op.read_start+op.read_len]
                reads.append(time.perf_counter_ns()-started)
                checksum=fnv_update(checksum,piece)
                if len(payload)!=workload.version_lengths[child_index]:
                    raise AssertionError(f"length mismatch at version {child_index}")
            verify_samples(graph,configs,workload,verify_samples_count)
            semantic_pass=True
            final_relation_bytes=schema_relation_bytes(db_uri,schema)
            rows=schema_row_counts(db_uri,schema)
        with PostgresSaver.from_conn_string(scoped) as saver:
            graph=build_graph(saver,mode,snapshot_frequency)
            for index in sample_indices(len(configs),verify_samples_count):
                started=time.perf_counter_ns()
                actual=graph.get_state(configs[index]).values.get("payload",b"")
                reopen.append(time.perf_counter_ns()-started)
                if actual!=materialize_version(workload,index):
                    raise AssertionError(f"reopen semantic mismatch at version {index}")
    except Exception as exc:
        semantic_error=f"{type(exc).__name__}: {exc}"
        final_relation_bytes=schema_relation_bytes(db_uri,schema)
        rows=schema_row_counts(db_uri,schema)
        reopen=[]
    growth=max(0,final_relation_bytes-base_relation_bytes)
    return ModeResult(mode,schema,semantic_pass,semantic_error,base_relation_bytes,final_relation_bytes,growth,growth/max(1,len(workload.ops)),summarize(writes),summarize(reads),summarize(reopen),f"{checksum:016x}",rows,capabilities,len(workload.ops),workload.logical_version_bytes)

def package_versions() -> dict[str,str]:
    out={}
    for name in ("langgraph","langgraph-checkpoint","langgraph-checkpoint-postgres","psycopg"):
        try: out[name]=importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError: out[name]="not-installed"
    return out

def print_result(r: ModeResult) -> None:
    print(f"mode: {r.mode}")
    print(f"  semantic: {'PASS' if r.semantic_pass else 'FAIL'}")
    if r.semantic_error: print(f"  semantic_error: {r.semantic_error}")
    print(f"  base_relation_mib: {r.base_relation_bytes/(1024*1024):.3f}")
    print(f"  final_relation_mib: {r.final_relation_bytes/(1024*1024):.3f}")
    print(f"  relation_growth_bytes_per_branch: {r.growth_bytes_per_branch:.1f}")
    print(f"  update_state_us p50/p95/p99: {r.write.p50_us:.3f} / {r.write.p95_us:.3f} / {r.write.p99_us:.3f}")
    print(f"  get_state_plus_slice_us p50/p95/p99: {r.get_state_and_slice.p50_us:.3f} / {r.get_state_and_slice.p95_us:.3f} / {r.get_state_and_slice.p99_us:.3f}")
    print(f"  reopen_get_state_us p50/p95/p99: {r.reopen_get_state.p50_us:.3f} / {r.reopen_get_state.p95_us:.3f} / {r.reopen_get_state.p99_us:.3f}")
    print(f"  checksum: {r.checksum}")
    print(f"  row_counts: {json.dumps(r.row_counts,sort_keys=True)}")
    print(f"  capabilities: {json.dumps(r.capabilities,sort_keys=True)}")

def parse_int(v: str) -> int:
    return int(v,16) if v.lower().startswith("0x") else int(v)

def main() -> int:
    p=argparse.ArgumentParser()
    p.add_argument("--db-uri",default=os.environ.get("TULYA_BENCH_DATABASE_URI","postgresql://postgres:postgres@localhost:55432/postgres"))
    p.add_argument("--mode",choices=("plain","delta","both"),default="both")
    p.add_argument("--workload",choices=("small-edit","append-heavy","linear-append","cross-template","large-rewrite"),default="small-edit")
    p.add_argument("--branches",type=int,default=1000)
    p.add_argument("--base-mib",type=int,default=2)
    p.add_argument("--base-kib",type=int)
    p.add_argument("--edit-bytes",type=int,default=96)
    p.add_argument("--read-bytes",type=int,default=4096)
    p.add_argument("--seed",type=parse_int,default=DEFAULT_SEED)
    p.add_argument("--snapshot-frequency",type=int,default=1000)
    p.add_argument("--verify-samples",type=int,default=16)
    p.add_argument("--schema-prefix",default="tulya_langgraph_bench")
    p.add_argument("--json-out",type=Path)
    a=p.parse_args()
    if a.branches<=0: p.error("--branches must be positive")
    if a.edit_bytes<0 or a.read_bytes<0: p.error("byte counts must be non-negative")
    if a.snapshot_frequency<=0: p.error("--snapshot-frequency must be positive")
    base_bytes=(a.base_kib*1024 if a.base_kib is not None else a.base_mib*1024*1024)
    versions=package_versions()
    print("LangGraph/PostgresSaver product baseline")
    print(f"packages: {json.dumps(versions,sort_keys=True)}")
    print(f"config: workload={a.workload}, branches={a.branches}, base_bytes={base_bytes}, edit_bytes={a.edit_bytes}, read_bytes={a.read_bytes}, seed=0x{a.seed:016x}, snapshot_frequency={a.snapshot_frequency}")
    w=generate_workload(a.workload,a.branches,base_bytes,a.edit_bytes,a.read_bytes,a.seed)
    print(f"logical bytes across retained versions: {w.logical_version_bytes} ({w.logical_version_bytes/(1024**3):.3f} GiB)")
    modes=("plain","delta") if a.mode=="both" else (a.mode,)
    results=[]
    for mode in modes:
        schema=f"{a.schema_prefix}_{mode}"
        print(f"\nrunning LangGraph PostgresSaver mode={mode} schema={schema}...")
        r=run_mode(db_uri=a.db_uri,schema=schema,mode=mode,workload=w,snapshot_frequency=a.snapshot_frequency,verify_samples_count=a.verify_samples)
        print_result(r); results.append(r)
    if len(results)==2:
        plain,delta=results
        print("\ncomparison (delta/plain; lower is better):")
        if plain.relation_growth_bytes: print(f"  relation growth ratio: {delta.relation_growth_bytes/plain.relation_growth_bytes:.3f}x")
        if plain.write.p95_us: print(f"  update_state p95 ratio: {delta.write.p95_us/plain.write.p95_us:.3f}x")
        if plain.get_state_and_slice.p95_us: print(f"  get_state+slice p95 ratio: {delta.get_state_and_slice.p95_us/plain.get_state_and_slice.p95_us:.3f}x")
        print(f"  semantic: plain={'PASS' if plain.semantic_pass else 'FAIL'}, delta={'PASS' if delta.semantic_pass else 'FAIL'}")
        print(f"  checksum agreement: {'PASS' if plain.checksum==delta.checksum else 'FAIL'} (plain={plain.checksum}, delta={delta.checksum})")
    if a.json_out:
        a.json_out.parent.mkdir(parents=True,exist_ok=True)
        payload={"packages":versions,"config":{"workload":a.workload,"branches":a.branches,"base_bytes":base_bytes,"edit_bytes":a.edit_bytes,"read_bytes":a.read_bytes,"seed":a.seed,"snapshot_frequency":a.snapshot_frequency,"verify_samples":a.verify_samples},"results":[asdict(r) for r in results]}
        a.json_out.write_text(json.dumps(payload,indent=2)+"\n")
        print(f"\nwrote {a.json_out}")
    return 2 if any(not r.semantic_pass for r in results) else 0

if __name__=="__main__":
    raise SystemExit(main())
