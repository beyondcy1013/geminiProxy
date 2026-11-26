#!/usr/bin/env python3
"""Stateful interactive mock agent for geminiProxy end-to-end verification."""
import sys


def main():
    memory = []
    turns = 0
    print("Mock Stateful Agent ready.", flush=True)
    while True:
        try:
            line = input("agent> ")
        except EOFError:
            break
        text = line.strip()
        if not text:
            continue
        turns += 1
        if text.startswith("REMEMBER:"):
            val = text[len("REMEMBER:"):].strip()
            memory.append(val)
            print(f"STORED: {val}", flush=True)
        elif text == "RECALL":
            joined = ", ".join(memory) if memory else "EMPTY"
            print(f"MEMORY: {joined}", flush=True)
        else:
            print(f"AGENT_REPLY[turn={turns}]: {text}", flush=True)


if __name__ == "__main__":
    main()
