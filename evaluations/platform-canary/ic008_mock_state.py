"""Thread-safe counters and content gate for the IC-008 provider fixture."""

from __future__ import annotations

import threading


class ProviderState:
    def __init__(self, blocking_content: str | None, gate_timeout_seconds: float) -> None:
        self._condition = threading.Condition()
        self._counts = {
            "chat_completions": 0,
            "effect_calls": 0,
            "final_responses": 0,
            "tool_call_responses": 0,
        }
        self.blocking_content = blocking_content
        self.gate_timeout_seconds = gate_timeout_seconds
        self.blocked_requests = 0
        self.release_generation = 0

    def increment(self, name: str, maximum: int) -> int | None:
        with self._condition:
            if self._counts[name] >= maximum:
                return None
            self._counts[name] += 1
            return self._counts[name]

    def counts(self) -> dict[str, int]:
        with self._condition:
            return dict(self._counts)

    def status(self) -> dict[str, int | bool]:
        with self._condition:
            return {
                **self._counts,
                "blocked": self.blocked_requests > 0,
                "blocked_requests": self.blocked_requests,
                "blocking_content_configured": self.blocking_content is not None,
                "release_generation": self.release_generation,
            }

    def wait_if_blocked(self, content: str) -> bool:
        if content != self.blocking_content:
            return True
        with self._condition:
            generation = self.release_generation
            self.blocked_requests += 1
            self._condition.notify_all()
            released = self._condition.wait_for(
                lambda: self.release_generation > generation,
                timeout=self.gate_timeout_seconds,
            )
            self.blocked_requests -= 1
            self._condition.notify_all()
            return released

    def release(self) -> tuple[int, int]:
        with self._condition:
            released = self.blocked_requests
            self.release_generation += 1
            generation = self.release_generation
            self._condition.notify_all()
            return released, generation
