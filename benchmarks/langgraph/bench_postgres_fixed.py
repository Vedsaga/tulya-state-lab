#!/usr/bin/env python3
"""Corrected entry point for the LangGraph/Postgres benchmark.

The original benchmark module uses ``from __future__ import annotations``.
A class-body annotation referring to the locally-created DeltaChannel was
therefore deferred as the string ``channel`` and later resolved from module
globals, causing ``NameError: name 'channel' is not defined`` before any delta
checkpoint was written.

This entry point reuses the benchmark unchanged except for building the state
schema with functional TypedDict syntax, which evaluates the Annotated metadata
immediately and keeps the actual DeltaChannel object attached to ``payload``.
"""
from __future__ import annotations

from typing import Annotated, Any
from typing_extensions import TypedDict

import bench_postgres as bench
from langgraph.channels import DeltaChannel
from langgraph.graph import END, START, StateGraph


def build_graph(saver, mode: str, snapshot_frequency: int):
    if mode == "plain":
        State = TypedDict(
            "State",
            {"payload": Annotated[bytes, bench.plain_reducer]},
        )
    elif mode == "delta":
        channel = DeltaChannel(
            bench.delta_reducer,
            bytes,
            snapshot_frequency=snapshot_frequency,
        )
        State = TypedDict(
            "State",
            {"payload": Annotated[bytes, channel]},
        )
    else:
        raise ValueError(mode)

    def bench_node(_state: State) -> dict[str, Any]:
        return {}

    builder = StateGraph(State)
    builder.add_node("bench", bench_node)
    builder.add_edge(START, "bench")
    builder.add_edge("bench", END)
    return builder.compile(checkpointer=saver)


bench.build_graph = build_graph

if __name__ == "__main__":
    raise SystemExit(bench.main())
