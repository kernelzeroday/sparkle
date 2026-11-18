#!/usr/bin/env python3
"""Copies the prompts in prompt-manifest.tsv into sparkle-core and regenerates the catalog.

Usage: scripts/vendor-prompts.py [LIBRARY_DIR]

LIBRARY_DIR defaults to ~/Documents/prompt-libraries. Source files are only read as text.
"""

import re
import shutil
import sys
import textwrap
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "scripts" / "prompt-manifest.tsv"
PROMPTS = ROOT / "crates" / "sparkle-core" / "prompts"
CATALOG = ROOT / "crates" / "sparkle-core" / "src" / "library_catalog.rs"

SOURCES = {
    "L": "system_prompts_leaks",
    "B": "TheBigPromptLibrary/SystemPrompts",
    "A": "awesome-system-prompts/LLMs",
    "C": "chatgpt_system_prompt/prompts/official-product",
}

FAMILIES = {
    "openai": "OpenAI",
    "anthropic": "Anthropic",
    "google": "Google",
    "xai": "xAI",
    "meta": "Meta",
    "mistral": "Mistral",
    "deepseek": "DeepSeek",
    "qwen": "Qwen",
    "moonshot": "Moonshot",
    "zhipu": "Zhipu",
    "perplexity": "Perplexity",
    "microsoft": "Microsoft",
}

# Collector notes around a prompt, as opposed to the prompt itself.
NOTE = re.compile(
    r"^\s*(source:|credit|contribute[d]? by|thanks to|\[?llama4 system prompt\]?|"
    r"this is .{0,40}system prompt|.{0,60}system prompt by |"
    r"https?://\S+\s*$|<!--.*-->\s*$)",
    re.IGNORECASE,
)
FENCE = re.compile(r"^\s*(```+|~~~+)")
EXTRACTION_NOTE = re.compile(
    r"^\s*(i used the following prompt|prompt used to|extracted (with|using))", re.IGNORECASE
)


# Where a prompt really starts, for captures that open with the collector's own notes.
CUT_BEFORE = {
    "xai/grok-2-2024-08": r"^\s*You are Grok 2",
    "xai/grok-2-2024-12": r"^\s*You are Grok 2",
    "meta/llama-4-whatsapp": r"^You are an expert conversationalist",
    "google/gemini-2.5-pro": r"^You are Gemini",
    "anthropic/claude-fable-5.2-full": r"^Claude should never use",
}

# Where a prompt really ends. What follows is the collector's account data or a second capture.
CUT_FROM = {
    "xai/grok-2-2024-08": r"^### Fun mode",
    "openai/gpt-5.5-thinking": r"^\[Message role: user, name: user_editable_context\]",
    "openai/gpt-5.2-thinking": r"^The user provided the following information about themselves",
    "xai/grok-4.5": r"^#+ User Info",
    "xai/grok-4.6": r"^#+ User Info",
    "xai/grok-4.7": r"^#+ User Info",
}

# Every capture stops before a memory snapshot, which describes the person who captured it.
MEMORY_SNAPSHOT = re.compile(r"^`?<user_memory_snapshot", re.MULTILINE)

# Lines describing the collector: where they were, and what their account was called.
PERSONAL_LINE = re.compile(
    r"^.*(current location is|Current location:|approximate location:|estimated location of|"
    r"User is currently in |^- Location: |Display Name:|X User Handle:|"
    r"Reykjav[ií]k|Hafnarfj|[ÁA]sgeir (is|Thor)|name is [ÁA]sgeir).*\n?",
    re.MULTILINE,
)

# The collector's home directory, inside paths the prompt still needs.
HOME_PATH = re.compile(r"/Users/asgeirtj\b")

# Personal details of known collectors. Vendoring stops if any survive the rules above.
DENYLIST = re.compile(
    r"[ÁA]sgeir|asgeirtj|Thor Johnson|Hafnarfj|Reykjav[ií]k|Lebanon, WA", re.IGNORECASE
)


def unwrap_fence(lines):
    """Returns the inside of a code fence that wraps nearly the whole file."""
    opening = next((i for i, line in enumerate(lines[:15]) if FENCE.match(line)), None)
    if opening is None:
        return lines
    closing = next(
        (i for i in range(len(lines) - 1, opening, -1) if FENCE.fullmatch(lines[i].strip())), None
    )
    if closing is None:
        return lines
    inside = sum(len(line) for line in lines[opening + 1 : closing])
    total = sum(len(line) for line in lines) or 1
    return lines[opening + 1 : closing] if inside / total >= 0.7 else lines


def clean(text):
    text = text.replace("\r\n", "\n").lstrip("﻿")
    if text.startswith("---\n") and (end := text.find("\n---\n", 4)) != -1:
        text = text[end + 5 :]
    lines = text.split("\n")
    for index, line in enumerate(lines):
        if EXTRACTION_NOTE.match(line):
            lines = lines[:index]
            break
    while lines and (not lines[0].strip() or NOTE.match(lines[0])):
        lines.pop(0)
    lines = unwrap_fence(lines)
    while lines and (not lines[0].strip() or NOTE.match(lines[0])):
        lines.pop(0)
    return "\n".join(lines).strip() + "\n"


def curate(prompt_id, text):
    """Applies the per-prompt rules and removes details about the person who made the capture."""
    if pattern := CUT_BEFORE.get(prompt_id):
        text = text[re.search(pattern, text, re.MULTILINE).start() :]
    if pattern := CUT_FROM.get(prompt_id):
        text = text[: re.search(pattern, text, re.MULTILINE).start()]
    if snapshot := MEMORY_SNAPSHOT.search(text):
        text = text[: snapshot.start()]
    text = HOME_PATH.sub("/Users/user", text)
    text = PERSONAL_LINE.sub("", text)
    text = re.sub(r"<br>[ \t]*$", "", text, flags=re.MULTILINE)
    # Cutting a capture short can leave the tag that opened the removed section.
    text = re.sub(r"\s*`<system-reminder>`\s*\Z", "\n", text)
    text = textwrap.dedent(text).strip() + "\n"
    if found := DENYLIST.search(text):
        sys.exit(f"{prompt_id} still contains personal data: {found.group()!r}")
    return text


def rust_string(value):
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def main():
    library = Path(sys.argv[1]) if len(sys.argv) > 1 else Path.home() / "Documents/prompt-libraries"
    rows = []
    for line in MANIFEST.read_text(encoding="utf-8").splitlines():
        if line.strip() and not line.startswith("#"):
            rows.append(line.split("\t"))

    if PROMPTS.exists():
        shutil.rmtree(PROMPTS)
    entries = []
    seen = {}
    for prompt_id, title, date, origin, source in rows:
        prefix, relative = source.split(":", 1)
        raw = (library / SOURCES[prefix] / relative).read_text(encoding="utf-8", errors="replace")
        text = curate(prompt_id, clean(raw))
        if text in seen:
            sys.exit(f"{prompt_id} has the same text as {seen[text]}")
        seen[text] = prompt_id
        target = PROMPTS / f"{prompt_id}.md"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
        entries.append((prompt_id, title, date, origin, SOURCES[prefix] + "/" + relative))

    output = [
        "// Generated by scripts/vendor-prompts.py. Do not edit by hand.",
        "",
        "use crate::library::{Origin, Prompt};",
        "",
        "pub static FAMILIES: &[(&str, &str)] = &[",
    ]
    used = {entry[0].split("/")[0] for entry in entries}
    output += [
        f"    ({rust_string(key)}, {rust_string(name)}),"
        for key, name in FAMILIES.items()
        if key in used
    ]
    output += ["];", "", "pub static PROMPTS: &[Prompt] = &["]
    for prompt_id, title, date, origin, source in entries:
        output += [
            "    Prompt {",
            f"        id: {rust_string(prompt_id)},",
            f"        title: {rust_string(title)},",
            f"        date: {rust_string(date)},",
            f"        origin: Origin::{origin.capitalize()},",
            f"        source: {rust_string(source)},",
            f'        text: include_str!("../prompts/{prompt_id}.md"),',
            "    },",
        ]
    output += ["];", ""]
    CATALOG.write_text("\n".join(output), encoding="utf-8")

    notice = [
        "# Vendored system prompts",
        "",
        "These texts belong to their respective vendors and are **not** covered by sparkle's",
        "Apache-2.0 license. `published` prompts were released by the vendor. `captured` prompts",
        "were extracted by the community and may be incomplete or inaccurate.",
        "",
        "Regenerate with `scripts/vendor-prompts.py`.",
        "",
        "| Prompt | Origin | Collected from |",
        "| --- | --- | --- |",
    ]
    notice += [f"| `{e[0]}` | {e[3]} | `{e[4]}` |" for e in entries]
    (PROMPTS / "README.md").write_text("\n".join(notice) + "\n", encoding="utf-8")
    total = sum(len(text.encode()) for text in seen)
    print(f"vendored {len(entries)} prompts, {total / 1024 / 1024:.1f} MB")


if __name__ == "__main__":
    main()
