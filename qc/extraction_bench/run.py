#!/usr/bin/env python3
"""Benchmark Hauksbee datasheet extraction across Codex model profiles."""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import tomllib
from datetime import datetime, timezone
from typing import Any, Iterable
from urllib.request import Request, urlopen


BENCH_DIR = Path(__file__).resolve().parent
REPO_ROOT = BENCH_DIR.parents[1]
CASES_FILE = BENCH_DIR / "cases.toml"
BINARY = REPO_ROOT / "target" / "release" / "hauksbee"
DATASHEETS_DIR = BENCH_DIR / "datasheets"
RUNS_DIR = BENCH_DIR / "runs"

PROFILE_MODELS = {
    "azure": "gpt-5.6-sol",
    "azure-luna": "gpt-5.6-luna",
    "azure-terra": "gpt-5.6-terra",
}
RATE_LIMIT_BACKOFF_SECONDS = (60, 180, 300)
ATTEMPT_RETURNED_RE = re.compile(
    r"\[model-extract\]\s+\S+\s+attempt\s+\d+\s+returned", re.IGNORECASE
)
ATTEMPT_ANY_RE = re.compile(r"\battempt\s+\d+\b", re.IGNORECASE)
SECTION_BASIS_RE = re.compile(
    r"\b(section|table|figure|page|electrical characteristics|"
    r"absolute maximum|recommended operating|device comparison|pin functions|"
    r"feature description)\b|\bp\.\s*\d+",
    re.IGNORECASE,
)
UNMODELLED_RE = re.compile(
    r"\b(unmodell?ed|not\s+modelled|not\s+modeled|does\s+not\s+model|"
    r"doesn't\s+model|omits?|unsupported|not\s+represented|excluded|"
    r"missing\s+behavio(?:u)?r)\b",
    re.IGNORECASE,
)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def load_cases() -> list[dict[str, Any]]:
    try:
        with CASES_FILE.open("rb") as handle:
            document = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise SystemExit(f"error: cannot load {CASES_FILE}: {exc}") from exc

    if document.get("schema_version") != 1:
        raise SystemExit(f"error: {CASES_FILE} must declare schema_version = 1")
    cases = document.get("cases")
    if not isinstance(cases, list) or not cases:
        raise SystemExit(f"error: {CASES_FILE} contains no [[cases]] entries")

    seen: set[str] = set()
    for case in cases:
        part = case.get("part")
        if not isinstance(part, str) or not part:
            raise SystemExit(f"error: every case in {CASES_FILE} needs a part")
        if part in seen:
            raise SystemExit(f"error: duplicate case {part} in {CASES_FILE}")
        seen.add(part)
        for field in ("url", "expected_kind"):
            if not isinstance(case.get(field), str) or not case[field]:
                raise SystemExit(f"error: case {part} needs {field}")
        facts = case.get("facts")
        if not isinstance(facts, list) or not facts:
            raise SystemExit(f"error: case {part} needs at least one [[cases.facts]] entry")
        for fact in facts:
            if not isinstance(fact.get("name"), str):
                raise SystemExit(f"error: case {part} has a fact without a name")
            value = fact.get("value")
            tolerance = fact.get("tolerance")
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                raise SystemExit(f"error: case {part} fact {fact['name']} has no numeric value")
            if (
                isinstance(tolerance, bool)
                or not isinstance(tolerance, (int, float))
                or tolerance < 0
            ):
                raise SystemExit(
                    f"error: case {part} fact {fact['name']} has an invalid tolerance"
                )
    return cases


def parse_args(cases: list[dict[str, Any]]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run the datasheet-to-model extraction benchmark. Each extraction sends "
            "TI datasheet text to the selected configured Codex backend."
        )
    )
    parser.add_argument(
        "--only",
        choices=tuple(PROFILE_MODELS),
        metavar="PROFILE",
        help="run only one profile: azure, azure-luna, or azure-terra",
    )
    parser.add_argument(
        "--case",
        type=str.upper,
        choices=tuple(case["part"] for case in cases),
        metavar="PART",
        help="run only one part",
    )
    parser.add_argument(
        "--repeat",
        type=int,
        default=1,
        metavar="N",
        help="independent samples per profile/case combination (default: 1)",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="rerun cells that already completed successfully",
    )
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error("--repeat must be at least 1")
    return args


def check_prerequisites() -> None:
    if not BINARY.is_file() or not os.access(BINARY, os.X_OK):
        raise SystemExit(
            "error: release binary missing or not executable at "
            f"{BINARY}\nBuild it separately before running this benchmark; the benchmark "
            "will not build it for you."
        )
    azure_env = Path.home() / ".azure-ai.env"
    if not azure_env.is_file():
        raise SystemExit(
            f"error: Azure credentials file is missing at {azure_env}; every extraction "
            "must source ~/.azure-ai.env first"
        )
    missing_profiles = [
        str(Path.home() / ".codex" / f"{profile}.config.toml")
        for profile in PROFILE_MODELS
        if not (Path.home() / ".codex" / f"{profile}.config.toml").is_file()
    ]
    if missing_profiles:
        raise SystemExit(
            "error: missing Codex profile configuration(s):\n  "
            + "\n  ".join(missing_profiles)
        )


def detect_lint() -> tuple[bool, str]:
    invocation = f"{BINARY} models lint <card.toml>"
    try:
        completed = subprocess.run(
            [str(BINARY), "models", "--help"],
            cwd=REPO_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            errors="replace",
            check=False,
        )
    except OSError:
        return False, invocation
    available = completed.returncode == 0 and bool(
        re.search(r"(?m)^\s*lint\s+", completed.stdout)
    )
    return available, invocation


def download_datasheet(case: dict[str, Any]) -> Path:
    DATASHEETS_DIR.mkdir(parents=True, exist_ok=True)
    destination = DATASHEETS_DIR / f"{case['part']}.pdf"
    if destination.exists():
        return destination

    request = Request(
        case["url"],
        headers={"User-Agent": "hauksbee-extraction-benchmark/1.0"},
    )
    partial = destination.with_suffix(".pdf.download")
    try:
        with urlopen(request, timeout=90) as response, partial.open("wb") as output:
            while chunk := response.read(1024 * 1024):
                output.write(chunk)
    except Exception as exc:
        raise RuntimeError(f"downloading {case['url']}: {exc}") from exc

    try:
        signature = partial.read_bytes()[:5]
    except OSError as exc:
        raise RuntimeError(f"reading downloaded {partial}: {exc}") from exc
    if signature != b"%PDF-":
        raise RuntimeError(f"downloaded file is not a PDF: {partial}")
    partial.replace(destination)
    return destination


def extraction_command(case: dict[str, Any], pdf: Path, out_dir: Path) -> list[str]:
    command = [
        str(BINARY),
        "models",
        "extract",
        "--pdf",
        str(pdf),
        "--part",
        case["part"],
    ]
    kind_arg = case.get("kind_arg")
    if isinstance(kind_arg, str) and kind_arg:
        command.extend(["--kind", kind_arg])
    command.extend(["--backend", "codex", "--out-dir", str(out_dir), "-y"])
    return command


def prepare_codex_home(run_dir: Path, profile: str) -> Path:
    """Give nested Codex a writable, run-local state dir with one known profile."""
    source = Path.home() / ".codex" / f"{profile}.config.toml"
    destination = run_dir / "codex-home"
    destination.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination / source.name)
    return destination


def shell_wrapped_command(
    profile: str, model: str, codex_home: Path, command: list[str]
) -> list[str]:
    # Positional parameters keep every path out of shell parsing. Set the profile and
    # model after sourcing so ~/.azure-ai.env cannot silently replace the cell's model.
    script = (
        'source "$HOME/.azure-ai.env" && '
        'export HAUKSBEE_CODEX_PROFILE="$1" HAUKSBEE_CODEX_MODEL="$2" '
        'CODEX_HOME="$3" && '
        "shift 3 && exec \"$@\""
    )
    return [
        "/bin/zsh",
        "-lc",
        script,
        "hauk-extraction-bench",
        profile,
        model,
        str(codex_home),
        *command,
    ]


def count_reported_attempts(outputs: Iterable[str]) -> int:
    outputs = list(outputs)
    returned = sum(len(ATTEMPT_RETURNED_RE.findall(output)) for output in outputs)
    if returned:
        return returned
    return sum(
        1
        for output in outputs
        for line in output.splitlines()
        if ATTEMPT_ANY_RE.search(line)
    )


def find_card(run_dir: Path, part: str) -> Path | None:
    preferred = run_dir / f"{part}.toml"
    if preferred.is_file():
        return preferred
    cards = sorted(path for path in run_dir.glob("*.toml") if path.is_file())
    return cards[0] if len(cards) == 1 else None


def walk_numbers(value: Any, path: str = "") -> Iterable[tuple[str, float]]:
    if isinstance(value, bool):
        return
    if isinstance(value, (int, float)) and math.isfinite(float(value)):
        yield path, float(value)
        return
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = f"{path}.{key}" if path else str(key)
            yield from walk_numbers(child, child_path)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            yield from walk_numbers(child, f"{path}[{index}]")


def fact_match(
    fact: dict[str, Any], numbers: list[tuple[str, float]]
) -> tuple[bool, dict[str, Any] | None]:
    expected = float(fact["value"])
    tolerance = float(fact["tolerance"])
    path_contains = fact.get("path_contains")
    for path, observed in numbers:
        if isinstance(path_contains, str) and path_contains.lower() not in path.lower():
            continue
        if abs(observed - expected) <= tolerance:
            return True, {"path": path, "observed": observed}
    return False, None


def score_numeric_facts(
    case: dict[str, Any], document: dict[str, Any]
) -> tuple[bool, str, dict[str, Any]]:
    numbers = list(walk_numbers(document))
    grouped: dict[str, dict[str, list[dict[str, Any]]]] = {}
    for fact in case["facts"]:
        assertion = str(fact.get("assertion", fact["name"]))
        alternative = str(fact.get("alternative", "required"))
        grouped.setdefault(assertion, {}).setdefault(alternative, []).append(fact)

    details: dict[str, Any] = {}
    failed_assertions: list[str] = []
    for assertion, alternatives in grouped.items():
        alternative_details: dict[str, Any] = {}
        assertion_passed = False
        for alternative, facts in alternatives.items():
            fact_details = []
            alternative_passed = True
            for fact in facts:
                passed, match = fact_match(fact, numbers)
                alternative_passed &= passed
                fact_details.append(
                    {
                        "name": fact["name"],
                        "expected": fact["value"],
                        "tolerance": fact["tolerance"],
                        "unit": fact.get("unit"),
                        "path_contains": fact.get("path_contains"),
                        "passed": passed,
                        "match": match,
                    }
                )
            alternative_details[alternative] = {
                "passed": alternative_passed,
                "facts": fact_details,
            }
            assertion_passed |= alternative_passed
        details[assertion] = {
            "passed": assertion_passed,
            "alternatives": alternative_details,
        }
        if not assertion_passed:
            failed_assertions.append(assertion)

    if failed_assertions:
        return False, "missing numeric assertion(s): " + ", ".join(failed_assertions), details
    return True, "all expected numeric assertions matched", details


def uncertainty_has_section_basis(models: list[dict[str, Any]]) -> bool:
    for model in models:
        source = model.get("source")
        if not isinstance(source, dict):
            continue
        uncertainty = source.get("uncertainty")
        if not isinstance(uncertainty, list):
            continue
        for entry in uncertainty:
            if not isinstance(entry, dict):
                continue
            basis = entry.get("basis")
            if isinstance(basis, str) and SECTION_BASIS_RE.search(basis):
                return True
    return False


def declares_unmodelled(models: list[dict[str, Any]], raw_card: str) -> bool:
    descriptions = "\n".join(
        model.get("description", "")
        for model in models
        if isinstance(model.get("description", ""), str)
    )
    comments = "\n".join(
        match.group(1) for match in re.finditer(r"(?m)#(.*)$", raw_card)
    )
    return bool(UNMODELLED_RE.search(descriptions) or UNMODELLED_RE.search(comments))


def criterion(index: int, label: str, status: str, note: str, **extra: Any) -> dict[str, Any]:
    result = {"index": index, "label": label, "status": status, "note": note}
    result.update(extra)
    return result


def score_card(
    case: dict[str, Any], card_path: Path | None, lint_available: bool
) -> dict[str, Any]:
    criteria: list[dict[str, Any]] = []
    produced = card_path is not None and card_path.is_file()
    criteria.append(
        criterion(
            1,
            "card produced",
            "pass" if produced else "fail",
            card_path.name if produced and card_path else "no card file was produced",
        )
    )

    raw_card = ""
    document: dict[str, Any] | None = None
    models: list[dict[str, Any]] = []
    parse_note = "no card to parse"
    if produced and card_path:
        try:
            raw_card = card_path.read_text(encoding="utf-8", errors="replace")
            parsed = tomllib.loads(raw_card)
            candidate_models = parsed.get("models")
            if isinstance(candidate_models, list) and candidate_models and all(
                isinstance(model, dict) for model in candidate_models
            ):
                document = parsed
                models = candidate_models
                parse_note = f"parsed {len(models)} [[models]] table(s)"
            else:
                parse_note = "TOML has no non-empty [[models]] array"
        except (OSError, tomllib.TOMLDecodeError) as exc:
            parse_note = f"TOML parse failed: {exc}"
    criteria.append(
        criterion(
            2,
            "valid TOML with [[models]]",
            "pass" if document is not None else "fail",
            parse_note,
        )
    )

    observed_kind = models[0].get("kind") if models else None
    kind_ok = observed_kind == case["expected_kind"]
    criteria.append(
        criterion(
            3,
            "expected kind",
            "pass" if kind_ok else "fail",
            f"expected {case['expected_kind']!r}, observed {observed_kind!r}",
        )
    )

    if document is not None:
        facts_ok, facts_note, facts_detail = score_numeric_facts(case, document)
    else:
        facts_ok, facts_note, facts_detail = False, "card did not parse", {}
    criteria.append(
        criterion(
            4,
            "expected numeric facts",
            "pass" if facts_ok else "fail",
            facts_note,
            assertions=facts_detail,
        )
    )

    basis_ok = uncertainty_has_section_basis(models)
    criteria.append(
        criterion(
            5,
            "section-cited uncertainty",
            "pass" if basis_ok else "fail",
            (
                "an uncertainty basis names a datasheet section/table/figure/page"
                if basis_ok
                else "no [[models.source.uncertainty]] basis names a datasheet section"
            ),
        )
    )

    unmodelled_ok = declares_unmodelled(models, raw_card) if raw_card else False
    criteria.append(
        criterion(
            6,
            "unmodelled behavior declared",
            "pass" if unmodelled_ok else "fail",
            (
                "description or TOML comment declares omitted behavior"
                if unmodelled_ok
                else "description/comments do not declare omitted behavior"
            ),
        )
    )

    lint_exit_code: int | None = None
    lint_output = ""
    if not lint_available:
        criteria.append(
            criterion(
                7,
                "hauksbee models lint",
                "na",
                "models lint is unavailable in this release binary",
            )
        )
    elif not produced or card_path is None:
        criteria.append(
            criterion(7, "hauksbee models lint", "fail", "no card to lint")
        )
    else:
        try:
            lint = subprocess.run(
                [str(BINARY), "models", "lint", str(card_path)],
                cwd=REPO_ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                errors="replace",
                check=False,
            )
            lint_exit_code = lint.returncode
            lint_output = lint.stdout
        except OSError as exc:
            lint_exit_code = 127
            lint_output = str(exc)
        criteria.append(
            criterion(
                7,
                "hauksbee models lint",
                "pass" if lint_exit_code == 0 else "fail",
                f"exit code {lint_exit_code}",
                output=lint_output[-4000:],
            )
        )

    score = sum(item["status"] == "pass" for item in criteria)
    denominator = sum(item["status"] != "na" for item in criteria)
    failed = next((item for item in criteria if item["status"] == "fail"), None)
    first_failed = (
        f"{failed['index']}. {failed['label']}: {failed['note']}"
        if failed
        else "all applicable criteria passed"
    )
    return {
        "score": score,
        "denominator": denominator,
        "first_failed": first_failed,
        "criteria": criteria,
        "lint_exit_code": lint_exit_code,
    }


def existing_success(run_dir: Path, part: str) -> tuple[dict[str, Any], Path] | None:
    result_path = run_dir / "result.json"
    try:
        previous = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if previous.get("exit_code") != 0:
        return None
    card_name = previous.get("card_file")
    card = run_dir / card_name if isinstance(card_name, str) and card_name else find_card(run_dir, part)
    if card is None or not card.is_file():
        return None
    return previous, card


def write_result(run_dir: Path, result: dict[str, Any]) -> None:
    (run_dir / "result.json").write_text(
        json.dumps(result, indent=2, sort_keys=False) + "\n", encoding="utf-8"
    )


def run_cell(
    case: dict[str, Any],
    profile: str,
    repeat_index: int,
    pdf: Path,
    force: bool,
    lint_available: bool,
) -> dict[str, Any]:
    model = PROFILE_MODELS[profile]
    run_dir = RUNS_DIR / profile / case["part"] / f"repeat-{repeat_index:03d}"
    run_dir.mkdir(parents=True, exist_ok=True)

    if not force:
        prior = existing_success(run_dir, case["part"])
        if prior is not None:
            previous, card = prior
            scored = score_card(case, card, lint_available)
            result = {
                **previous,
                "skipped": True,
                "rescored_utc": utc_now(),
                "score": scored,
            }
            write_result(run_dir, result)
            return result

    command = extraction_command(case, pdf, run_dir)
    codex_home = prepare_codex_home(run_dir, profile)
    wrapped = shell_wrapped_command(profile, model, codex_home, command)
    log_path = run_dir / "extract.log"
    prior_card = find_card(run_dir, case["part"])
    prior_card_mtime = prior_card.stat().st_mtime_ns if prior_card else None
    outputs: list[str] = []
    rate_limit_retries = 0
    exit_code = 127
    started_utc = utc_now()
    started = time.monotonic()

    with log_path.open("w", encoding="utf-8") as log:
        for invocation_index in range(1 + len(RATE_LIMIT_BACKOFF_SECONDS)):
            log.write(
                f"===== extraction invocation {invocation_index + 1} "
                f"(profile={profile}, model={model}) =====\n"
            )
            log.write("$ " + " ".join(command) + "\n")
            log.flush()
            try:
                completed = subprocess.run(
                    wrapped,
                    cwd=REPO_ROOT,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    errors="replace",
                    check=False,
                )
                exit_code = completed.returncode
                output = completed.stdout
            except OSError as exc:
                exit_code = 127
                output = f"failed to launch extraction: {exc}\n"
            outputs.append(output)
            log.write(output)
            if output and not output.endswith("\n"):
                log.write("\n")
            log.flush()

            rate_limited = exit_code != 0 and "exceeded rate limit" in output.lower()
            if exit_code == 0 or not rate_limited:
                break
            if invocation_index >= len(RATE_LIMIT_BACKOFF_SECONDS):
                break
            delay = RATE_LIMIT_BACKOFF_SECONDS[invocation_index]
            rate_limit_retries += 1
            log.write(f"rate limit detected; retrying after {delay}s\n")
            log.flush()
            time.sleep(delay)

    wall_seconds = time.monotonic() - started
    card = find_card(run_dir, case["part"]) if exit_code == 0 else None
    if (
        card is not None
        and prior_card is not None
        and card == prior_card
        and prior_card_mtime == card.stat().st_mtime_ns
    ):
        card = None
    scored = score_card(case, card, lint_available)
    result = {
        "profile": profile,
        "model": model,
        "part": case["part"],
        "repeat_index": repeat_index,
        "skipped": False,
        "started_utc": started_utc,
        "command": command,
        "exit_code": exit_code,
        "wall_seconds": round(wall_seconds, 3),
        "attempts": count_reported_attempts(outputs),
        "rate_limit_retries": rate_limit_retries,
        "extract_log": "extract.log",
        "card_file": card.name if card else None,
        "score": scored,
    }
    write_result(run_dir, result)
    return result


def failed_cell(
    case: dict[str, Any],
    profile: str,
    repeat_index: int,
    message: str,
    lint_available: bool,
) -> dict[str, Any]:
    run_dir = RUNS_DIR / profile / case["part"] / f"repeat-{repeat_index:03d}"
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "extract.log").write_text(message + "\n", encoding="utf-8")
    scored = score_card(case, None, lint_available)
    scored["first_failed"] = f"setup failure: {message}"
    result = {
        "profile": profile,
        "model": PROFILE_MODELS[profile],
        "part": case["part"],
        "repeat_index": repeat_index,
        "skipped": False,
        "started_utc": utc_now(),
        "command": None,
        "exit_code": 125,
        "wall_seconds": 0.0,
        "attempts": 0,
        "rate_limit_retries": 0,
        "extract_log": "extract.log",
        "card_file": None,
        "score": scored,
    }
    write_result(run_dir, result)
    return result


def markdown_table(results: list[dict[str, Any]]) -> str:
    grouped: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for result in results:
        grouped.setdefault((result["part"], result["profile"]), []).append(result)

    lines = [
        "| Case | Profile | Score | Attempts | Wall seconds | Exit code | First failed criterion |",
        "|---|---|---:|---:|---:|---:|---|",
    ]
    for (part, profile), repeats in grouped.items():
        repeats.sort(key=lambda item: item["repeat_index"])
        scores = ", ".join(
            f"{item['score']['score']}/{item['score']['denominator']}" for item in repeats
        )
        attempts = ", ".join(str(item["attempts"]) for item in repeats)
        walls = ", ".join(f"{item['wall_seconds']:.1f}" for item in repeats)
        exits = ", ".join(str(item["exit_code"]) for item in repeats)
        first_failure = next(
            (
                item["score"]["first_failed"]
                for item in repeats
                if not item["score"]["first_failed"].startswith("all applicable")
            ),
            "all applicable criteria passed",
        )
        first_failure = first_failure.replace("|", "\\|").replace("\n", " ")
        lines.append(
            f"| {part} | {profile} | {scores} | {attempts} | {walls} | {exits} | {first_failure} |"
        )
    return "\n".join(lines)


def main() -> int:
    cases = load_cases()
    args = parse_args(cases)
    check_prerequisites()
    lint_available, lint_invocation = detect_lint()

    selected_cases = [case for case in cases if args.case is None or case["part"] == args.case]
    selected_profiles = [args.only] if args.only else list(PROFILE_MODELS)
    RUNS_DIR.mkdir(parents=True, exist_ok=True)

    downloads: dict[str, Path | str] = {}
    for case in selected_cases:
        try:
            downloads[case["part"]] = download_datasheet(case)
        except Exception as exc:
            downloads[case["part"]] = str(exc)

    results: list[dict[str, Any]] = []
    for case in selected_cases:
        for profile in selected_profiles:
            for repeat_index in range(1, args.repeat + 1):
                download = downloads[case["part"]]
                if isinstance(download, str):
                    result = failed_cell(
                        case, profile, repeat_index, download, lint_available
                    )
                else:
                    print(
                        f"running {case['part']} / {profile} / repeat {repeat_index}",
                        file=sys.stderr,
                        flush=True,
                    )
                    try:
                        result = run_cell(
                            case,
                            profile,
                            repeat_index,
                            download,
                            args.force,
                            lint_available,
                        )
                    except Exception as exc:
                        result = failed_cell(
                            case,
                            profile,
                            repeat_index,
                            f"benchmark harness failure: {exc}",
                            lint_available,
                        )
                results.append(result)

    summary = {
        "schema_version": 1,
        "generated_utc": utc_now(),
        "repository_root": str(REPO_ROOT),
        "binary": str(BINARY),
        "cases_file": str(CASES_FILE),
        "profile_models": PROFILE_MODELS,
        "lint_available": lint_available,
        "lint_invocation": lint_invocation if lint_available else None,
        "repeat": args.repeat,
        "results": results,
    }
    (RUNS_DIR / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=False) + "\n", encoding="utf-8"
    )
    print(markdown_table(results))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
