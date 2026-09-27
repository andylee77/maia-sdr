"""Exit codes, verdicts and the exception hierarchy.

Exit codes (design doc 5.1): 0 pass, 1 fail, 2 error (bug/exception),
3 precondition (unreachable unit, wrong image, missing capability),
4 safety refusal, 5 inconclusive.
"""

from __future__ import annotations

from enum import IntEnum


class ExitCode(IntEnum):
    PASS = 0
    FAIL = 1
    ERROR = 2
    PRECONDITION = 3
    SAFETY = 4
    INCONCLUSIVE = 5


#: result.json verdict strings and their exit codes.
VERDICT_EXIT: dict[str, int] = {
    "pass": ExitCode.PASS,
    "fail": ExitCode.FAIL,
    "error": ExitCode.ERROR,
    "precondition": ExitCode.PRECONDITION,
    "refused": ExitCode.SAFETY,
    "inconclusive": ExitCode.INCONCLUSIVE,
}

VERDICTS: tuple[str, ...] = tuple(VERDICT_EXIT)

#: Severity used to aggregate several runs (suites): the highest wins.
#: Safety refusals dominate, then errors, real failures, preconditions,
#: inconclusive results and finally passes.
_SEVERITY: dict[int, int] = {
    ExitCode.PASS: 0,
    ExitCode.INCONCLUSIVE: 1,
    ExitCode.PRECONDITION: 2,
    ExitCode.FAIL: 3,
    ExitCode.ERROR: 4,
    ExitCode.SAFETY: 5,
}


def worst_exit(codes: list[int]) -> int:
    """Aggregate exit codes of several runs (empty list -> 0)."""
    if not codes:
        return int(ExitCode.PASS)
    return int(max(codes, key=lambda c: _SEVERITY.get(c, 4)))


def exit_to_verdict(code: int) -> str:
    for verdict, c in VERDICT_EXIT.items():
        if c == code:
            return verdict
    return "error"


class FbenchError(Exception):
    """Base error; ``exit_code`` decides the process exit status."""

    exit_code: int = ExitCode.ERROR
    verdict: str = "error"

    def __init__(self, message: str, **details: object) -> None:
        super().__init__(message)
        self.message = message
        self.details = details

    def to_dict(self) -> dict:
        out: dict = {"error": self.message, "kind": type(self).__name__}
        if self.details:
            out["details"] = {k: v for k, v in self.details.items()}
        return out


class UsageError(FbenchError):
    """Bad command-line usage (unknown test, bad parameter)."""

    exit_code = ExitCode.ERROR


class ConfigError(FbenchError):
    exit_code = ExitCode.PRECONDITION
    verdict = "precondition"


class PreconditionError(FbenchError):
    exit_code = ExitCode.PRECONDITION
    verdict = "precondition"


class TransportError(PreconditionError):
    """Unit unreachable (SSH connect failure, HTTP/IIO connection refused)."""


class TransportTimeout(FbenchError):
    """A remote command did not finish within its timeout."""

    exit_code = ExitCode.ERROR


class SafetyRefusal(FbenchError):
    exit_code = ExitCode.SAFETY
    verdict = "refused"


class Inconclusive(FbenchError):
    exit_code = ExitCode.INCONCLUSIVE
    verdict = "inconclusive"


class AgentError(FbenchError):
    """The agent answered ``{"ok": false}`` or produced no JSON."""


class AgentRefused(SafetyRefusal):
    """The agent refused an operation (register allow-list, TX interlock)."""


class AgentUnsupported(PreconditionError):
    """The agent lacks the subcommand or is not deployed."""
