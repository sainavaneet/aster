from __future__ import annotations

import re
import unittest
from pathlib import Path


PROJECT_ROOT = Path(__file__).resolve().parent.parent
CHROME_DIR = PROJECT_ROOT / "src" / "ui" / "chrome"
MODERN_DIR = PROJECT_ROOT / "src" / "client" / "shell" / "modern"
SHELL_TESTS_DIR = PROJECT_ROOT / "src" / "client" / "shell" / "tests"
HOT_PATH_SOURCES = (
    PROJECT_ROOT / "src" / "ui.rs",
    *sorted((PROJECT_ROOT / "src" / "ui").rglob("*.rs")),
    # Modern chrome models and draw code run inside the shell's frame path.
    *sorted(MODERN_DIR.rglob("*.rs")),
    PROJECT_ROOT / "src" / "server" / "render_stream.rs",
)
# Modern per-frame code: it may only write borrowed text into a buffer.
DRAW_SOURCES = (
    CHROME_DIR / "draw.rs",
    CHROME_DIR / "age.rs",
    *sorted((MODERN_DIR / "draw").rglob("*.rs")),
)
# Files that may select between classic and modern, one per surface (section
# 6.1 of docs/design/aster-visual-redesign.md), plus the config plumbing.
STYLE_DISPATCH_FILES = frozenset(
    "src/client/shell/" + name
    for name in (
        "agent_sidebar.rs",
        "composition.rs",
        "config.rs",
        "endpoint_sidebar.rs",
        "global_menu.rs",
        "mobile.rs",
        "notifications.rs",
        "overlays.rs",
        "render.rs",
        "settings.rs",
        "sidebar.rs",
        "state.rs",
        "tabs.rs",
        "team_sidebar.rs",
    )
) | {"src/client/shell.rs", "src/cli/orbit_activity.rs"}
APP_SERVER_SOURCES = (
    *sorted((PROJECT_ROOT / "src" / "app").rglob("*.rs")),
    *sorted((PROJECT_ROOT / "src" / "server").rglob("*.rs")),
)
TEST_MODULE = re.compile(r"(?m)^[ \t]*#\[cfg\(test\)\]\s*(?:#\[[^\n]*\]\s*)*mod\s+\w+\s*\{")
INPUT_STATE_CALL = re.compile(r"(?:\.|::)input_state\b")
KEYBOARD_STATE_ANSI_CALL = re.compile(
    r"(?:\.|::)(?:keyboard_state_ansi|kitty_keyboard_state_ansi)\b"
)
AGGREGATE_STATE_CALLS = (
    (INPUT_STATE_CALL, "aggregate terminal input state; add a narrow accessor"),
    (KEYBOARD_STATE_ANSI_CALL, "formatted keyboard state"),
)
FORBIDDEN_CALLS = (
    *AGGREGATE_STATE_CALLS,
    (
        re.compile(r"(?:\.|::)screen_text_snapshot\b"),
        "formatted terminal screen snapshot",
    ),
    (
        re.compile(r"\bforeground_job\s*\("),
        "process-tree inspection",
    ),
)


DRAW_FORBIDDEN = (
    (re.compile(r"\bformat!\s*\("), "format! allocates"),
    (re.compile(r"\.to_string\s*\("), "to_string allocates"),
    (re.compile(r"\.to_owned\s*\("), "to_owned allocates"),
    (re.compile(r"\.clone\s*\("), "clone in a draw path"),
    (re.compile(r"\bvec!\s*[\[(]"), "vec! allocates"),
    (re.compile(r"\bString\s*::"), "String construction"),
    (re.compile(r"\bVec\s*::"), "Vec construction"),
    (re.compile(r"\.collect\s*[(:<]"), "collect allocates"),
    (re.compile(r"\bBox\s*::\s*new\b"), "Box allocation"),
    (re.compile(r"\b(?:HashMap|HashSet)\s*::"), "hash collection construction"),
    (re.compile(r"\bstd\s*::\s*fs\b"), "filesystem I/O"),
    (re.compile(r"\bInstant\s*::\s*now\b"), "clock read"),
    (re.compile(r"\bSystemTime\b"), "wall-clock read"),
)
STYLE_LOOKUPS = (
    (re.compile(r"\bmodern\s*::"), "modern style path"),
    (re.compile(r"\.chrome\b"), "style/glyph/role lookup"),
    (re.compile(r"\bui\s*::\s*chrome\s*::\s*(?:roles|glyphs|Chrome|ChromeRoles|GlyphSet)\b"), "direct chrome role/glyph import"),
)


def blank_non_newlines(chars: list[str], start: int, end: int) -> None:
    for index in range(start, end):
        if chars[index] != "\n":
            chars[index] = " "


def mask_comments_and_literals(source: str) -> str:
    chars = list(source)
    index = 0
    while index < len(source):
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            end = len(source) if end == -1 else end
            blank_non_newlines(chars, index, end)
            index = end
            continue

        if source.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(source) and depth > 0:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            blank_non_newlines(chars, index, end)
            index = end
            continue

        if source[index] == "r":
            quote = index + 1
            while quote < len(source) and source[quote] == "#":
                quote += 1
            if quote < len(source) and source[quote] == '"':
                suffix = '"' + "#" * (quote - index - 1)
                end = source.find(suffix, quote + 1)
                end = len(source) if end == -1 else end + len(suffix)
                blank_non_newlines(chars, index, end)
                index = end
                continue

        if source[index] == '"':
            end = index + 1
            while end < len(source):
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            blank_non_newlines(chars, index, min(end, len(source)))
            index = end
            continue

        if source[index] == "'":
            end = index + 2
            if index + 1 < len(source) and source[index + 1] == "\\":
                end += 1
            if end < len(source) and source[end] == "'":
                end += 1
                blank_non_newlines(chars, index, end)
                index = end
                continue

        index += 1

    return "".join(chars)


def production_code(source: str) -> str:
    code = mask_comments_and_literals(source)
    chars = list(code)
    search_from = 0

    while test_module := TEST_MODULE.search(code, search_from):
        depth = 0
        end = test_module.end() - 1
        while end < len(code):
            if code[end] == "{":
                depth += 1
            elif code[end] == "}":
                depth -= 1
                if depth == 0:
                    end += 1
                    break
            end += 1
        blank_non_newlines(chars, test_module.start(), end)
        code = "".join(chars)
        search_from = end

    return code


def find_violations(paths, rules) -> list[str]:
    violations: list[str] = []
    for path in paths:
        code = production_code(path.read_text(encoding="utf-8"))
        for pattern, description in rules:
            for match in pattern.finditer(code):
                line = code.count("\n", 0, match.start()) + 1
                try:
                    relative_path = path.relative_to(PROJECT_ROOT)
                except ValueError:
                    relative_path = path
                violations.append(f"{relative_path}:{line}: {description}")
    return violations


def style_lookup_violations(paths) -> list[str]:
    """Style lookups outside the files that own a dispatch point."""
    violations: list[str] = []
    for path in paths:
        relative = path.relative_to(PROJECT_ROOT).as_posix()
        if (
            relative in STYLE_DISPATCH_FILES
            or path.is_relative_to(CHROME_DIR)
            or path.is_relative_to(MODERN_DIR)
            # Test fixtures may inspect the resolved style; they never draw.
            or path.is_relative_to(SHELL_TESTS_DIR)
        ):
            continue
        violations.extend(find_violations([path], STYLE_LOOKUPS))
    return violations


class UiHotPathArchitectureTests(unittest.TestCase):
    def test_render_hot_paths_avoid_known_expensive_runtime_queries(self) -> None:
        violations = find_violations(HOT_PATH_SOURCES, FORBIDDEN_CALLS)

        self.assertEqual(
            violations,
            [],
            "Render/layout code must not perform pane-scaled expensive reads:\n"
            + "\n".join(violations),
        )

    def test_app_and_server_avoid_aggregate_terminal_state(self) -> None:
        self.assertTrue(APP_SERVER_SOURCES, "No app/server Rust sources were discovered")
        violations = find_violations(APP_SERVER_SOURCES, AGGREGATE_STATE_CALLS)

        self.assertEqual(
            violations,
            [],
            "App/server code must use narrow terminal-state accessors:\n"
            + "\n".join(violations),
        )

    def test_chrome_and_modern_sources_are_runtime_query_checked(self) -> None:
        checked = {path.relative_to(PROJECT_ROOT).as_posix() for path in HOT_PATH_SOURCES}
        for path in (
            *sorted(CHROME_DIR.rglob("*.rs")),
            *sorted(MODERN_DIR.rglob("*.rs")),
        ):
            self.assertIn(path.relative_to(PROJECT_ROOT).as_posix(), checked)

    def test_runtime_query_check_catches_a_chrome_or_modern_violation(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            for name, call in (
                ("input_state.rs", "fn draw() { runtime.input_state(); }"),
                ("job.rs", "fn draw() { foreground_job(pid); }"),
                ("snapshot.rs", "fn draw() { rt.screen_text_snapshot(); }"),
            ):
                path = Path(tmp) / name
                path.write_text(call)
                self.assertEqual(
                    len(find_violations([path], FORBIDDEN_CALLS)), 1, call
                )

    def test_modern_draw_code_does_not_allocate_or_do_io(self) -> None:
        self.assertTrue(all(path.exists() for path in DRAW_SOURCES))
        violations = find_violations(DRAW_SOURCES, DRAW_FORBIDDEN)

        self.assertEqual(
            violations,
            [],
            "Per-frame chrome drawing must only write borrowed text into a buffer:\n"
            + "\n".join(violations),
        )

    def test_style_lookups_only_appear_at_listed_dispatch_points(self) -> None:
        sources = sorted((PROJECT_ROOT / "src").rglob("*.rs"))
        violations = style_lookup_violations(sources)

        self.assertEqual(
            violations,
            [],
            "A surface reads the style outside section 6.1's dispatch table; "
            "list it there and in STYLE_DISPATCH_FILES:\n" + "\n".join(violations),
        )

    def test_draw_scanner_catches_each_forbidden_construct(self) -> None:
        cases = (
            'fn draw() { let s = format!("{x}"); }',
            "fn draw() { x.to_string(); }",
            "fn draw() { x.to_owned(); }",
            "fn draw() { x.clone(); }",
            "fn draw() { let v = vec![1]; }",
            "fn draw() { String::new(); }",
            "fn draw() { Vec::new(); }",
            "fn draw() { it.collect::<Vec<_>>(); }",
            "fn draw() { Box::new(1); }",
            "fn draw() { HashMap::new(); }",
            "fn draw() { std::fs::read(p); }",
            "fn draw() { Instant::now(); }",
            "fn draw() { SystemTime::now(); }",
        )
        for source in cases:
            with self.subTest(source=source):
                code = production_code(source)
                self.assertTrue(
                    any(pattern.search(code) for pattern, _ in DRAW_FORBIDDEN),
                    source,
                )

    def test_draw_scanner_ignores_tests_comments_and_literals(self) -> None:
        source = '''
// x.clone()
const NOTE: &str = "format!(x)";
#[cfg(test)]
mod tests {
    fn helper() { let v = vec![1]; Vec::new(); }
}
fn draw(buffer: &mut Buffer) { put_text(buffer, 0, 0, 1, "x", style); }
'''
        code = production_code(source)
        self.assertFalse(any(pattern.search(code) for pattern, _ in DRAW_FORBIDDEN))

    def test_style_scanner_catches_modern_paths_and_chrome_lookups(self) -> None:
        code = production_code("fn f() { config.chrome.roles; modern :: draw(); crate::ui :: chrome :: roles::derive(); }")
        for pattern, _ in STYLE_LOOKUPS:
            self.assertRegex(code, pattern)

    def test_draw_scanner_ignores_indented_attributed_test_module(self) -> None:
        source = """
    #[cfg(test)]
    #[allow(dead_code)]
    mod tests { fn example() { Vec::new(); } }
fn draw() { Vec::new(); }
"""
        code = production_code(source)
        self.assertEqual(code.count("Vec::new"), 1)
        self.assertEqual(code.count("\n"), source.count("\n"))

    def test_scanner_ignores_non_production_references(self) -> None:
        source = '''
// runtime.input_state()
const EXAMPLE: &str = "runtime.input_state()";
#[cfg(test)]
mod tests {
    fn aggregate_state_test() { runtime.input_state(); }
}
fn production_after_tests() {}
'''
        code = production_code(source)
        self.assertNotRegex(code, FORBIDDEN_CALLS[0][0])
        self.assertIn("fn production_after_tests()", code)
        self.assertEqual(code.count("\n"), source.count("\n"))

    def test_scanner_checks_production_after_test_modules(self) -> None:
        source = '''
#[cfg(test)]
mod tests {
    const BRACES: &str = "}}";
}
fn render() { TerminalRuntime::input_state; }
'''
        self.assertRegex(production_code(source), FORBIDDEN_CALLS[0][0])

    def test_scanner_catches_each_aggregate_state_call(self) -> None:
        cases = (
            ("fn render() { runtime.input_state(); }", INPUT_STATE_CALL),
            ("fn render() { runtime.keyboard_state_ansi(); }", KEYBOARD_STATE_ANSI_CALL),
            ("fn render() { runtime.kitty_keyboard_state_ansi(); }", KEYBOARD_STATE_ANSI_CALL),
        )
        for source, pattern in cases:
            with self.subTest(source=source):
                self.assertRegex(production_code(source), pattern)

    def test_scanner_catches_imported_process_query(self) -> None:
        source = "fn render() { foreground_job(pid); }"
        self.assertRegex(production_code(source), FORBIDDEN_CALLS[3][0])


if __name__ == "__main__":
    unittest.main()
