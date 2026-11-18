#!/usr/bin/env python3
"""Builds the local prompt pack that `/prompts` shows alongside the built-in library.

Usage: scripts/build-prompt-pack.py [--library DIR] [--out DIR] [--offline]

The pack holds third-party text under many licenses, so it stays on this machine and is never
committed. Every source file is read as data. Each one ends up in the pack, or in REPORT.tsv with
the reason it was left out.
"""

import argparse
import csv
import hashlib
import importlib.util
import json
import os
import re
import shutil
import sys
import urllib.request
from collections import Counter
from pathlib import Path

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parent.parent
BUILT_IN = ROOT / "crates" / "sparkle-core" / "prompts"

_spec = importlib.util.spec_from_file_location("vendor_prompts", ROOT / "scripts/vendor-prompts.py")
vendor = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(vendor)

UNCATEGORIZED = "Uncategorized"
MAX_BYTES = 2_500_000
MIN_BYTES = 60

# Whole collections that stay out, with the reason recorded in the report.
EXCLUDED_COLLECTIONS = {
    "L1B3RT45": "jailbreak collection",
    "Jb-Prompts-Ai": "jailbreak collection",
    "nsfw-ai-prompts": "sexual content",
    "erp-refusal-test": "sexual content",
    "character-archive": "roleplay cards",
    "character-card-skills": "roleplay cards",
    "sillytavern-card-library": "roleplay cards",
    "RP-Leaderboard": "roleplay cards",
    "poppet-rp-framework": "roleplay cards",
    "role-playing": "roleplay cards",
    "AI-roleplay-prompts": "roleplay cards",
    "Corvus-Story-Core": "roleplay cards",
    "Frankkie": "roleplay cards",
    "Gemini-Roleplay-Core": "roleplay cards",
    "LLM-Dungeon-Master-Preset": "roleplay cards",
    "ST-Presets": "roleplay cards",
    "AI-as-role-master": "roleplay cards",
    "grok-prompts": "user prompt examples, not system prompts",
    "Prompt-Engineering-Guide": "documentation",
    "anthropic-courses": "documentation",
    "claude-cookbooks": "documentation",
}

VENDOR_NAMES = {
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
    "minimax": "MiniMax",
    "perplexity": "Perplexity",
    "microsoft": "Microsoft",
}

# The first folder of a vendor-organized collection, mapped to the vendor it belongs to.
VENDOR_FOLDERS = {
    "openai": "openai",
    "chatgpt": "openai",
    "anthropic": "anthropic",
    "claude": "anthropic",
    "google": "google",
    "gemini": "google",
    "xai": "xai",
    "grok": "xai",
    "meta": "meta",
    "meta.ai": "meta",
    "mistral": "mistral",
    "deepseek": "deepseek",
    "qwen": "qwen",
    "alibaba": "qwen",
    "kimi": "moonshot",
    "moonshot": "moonshot",
    "glm": "zhipu",
    "zai": "zhipu",
    "z.ai code": "zhipu",
    "chatglm": "zhipu",
    "minimax": "minimax",
    "perplexity": "perplexity",
    "perplexity.ai": "perplexity",
    "microsoft": "microsoft",
    "copilot": "microsoft",
}

NOT_A_PROMPT_NAME = re.compile(
    r"^(readme|license|licence|contributing|changelog|code_of_conduct|toc|index|requirements|"
    r"review_notes|gpts_list|notice|security)([._-].*)?$",
    re.IGNORECASE,
)
NOT_A_PROMPT_PATH = re.compile(
    r"/(skills?|docs?|references?|artifact-types|anthropic-usage-policies|claude-constitution|raw|"
    r"memory|subscriptions|modules|assets|imgs|scripts|tests|site|catalog|i18n|auxiliary|"
    r"claude-docs)/|/official/all\.md$|/tools?\.(md|txt|json)$",
    re.IGNORECASE,
)
COMPANION_PATH = re.compile(r"companion|grok-personas|persona_(loyal|unhinged)|santa_mode", re.I)

JAILBREAK = re.compile(
    r"do anything now|jailbr(eak|oken)|developer mode (enabled|activated)|"
    r"ignore (all |any )?(of )?(your |the )?(previous|prior|above) (instructions|rules|prompts)|"
    r"(no|without( any)?|free (of|from)) (ethical|moral|content|safety) "
    r"(guidelines|restrictions|constraints|filters|limits|boundaries)|"
    r"\bunfiltered\b|\buncensored\b|\bamoral\b|never refuse|bypass(es|ing)? (the |all |any )?"
    r"(safety|content|filter|openai|restriction|guideline)|越狱|破限|无限制|无视.{0,6}(规则|限制)",
    re.IGNORECASE,
)
SEXUAL = re.compile(
    r"\bnsfw\b|\berotic|\bporn|\bhentai\b|\bsexually explicit\b|\bexplicit sex|\bwaifu\b|"
    r"\bgirlfriend\b|\bboyfriend\b|\bsexting\b|\bfetish|\bsmut\b|\br18\b|\bsexy\b|\blewd\b|"
    r"色情|黄文|成人内容|女友|男友",
    re.IGNORECASE,
)
HARMFUL = re.compile(
    r"\bmalware\b|\bransomware\b|\bkeylogger\b|\bbotnet\b|\bddos\b|\bcarding\b|"
    r"\bphishing (kit|page|email generator)\b|\bexploit (generator|builder)\b|"
    r"\bcredential stuffing\b|\bstealer\b",
    re.IGNORECASE,
)

SAFE_EMAIL_DOMAIN = re.compile(
    r"^(example|test|domain|company|acme|email|mail|yourdomain|yourcompany|sample|workspacesamples|"
    r"anthropic|openai|google|x|xai|mistral|microsoft|github|users\.noreply\.github|meta|"
    r"perplexity|deepseek|noreply)\.[a-z.]+$",
    re.IGNORECASE,
)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@([A-Za-z0-9-]+\.)+[A-Za-z]{2,}")
HOME_DIR = re.compile(r"/(?:Users|home)/([A-Za-z0-9._-]+)")
GENERIC_HOMES = {
    "user", "username", "ubuntu", "claude", "root", "runner", "you", "me", "oai", "sandbox",
    "workspace", "coder", "node", "app", "project", "jovyan", "devin", "daytona", "name",
    "admin", "dev", "developer", "agent", "alice", "bob", "john", "jane", "foo", "test", "repo",
    "yourname", "your", "kimi", "grok", "manus", "computeruse", "swebot", "workdir", "model",
    "alex", "hero", "living", "sam", "jdoe", "johndoe", "demo", "guest", "pi", "ec2-user",
}
PHONE = re.compile(r"(?<![\d.])\+\d{1,3}[ -]\(?\d{2,4}\)?[ -]\d{3,4}[ -]\d{3,4}(?!\d)")
SECRET = re.compile(r"\b(sk-[A-Za-z0-9]{24,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{30,}|xox[bp]-[0-9A-Za-z-]{20,})")
ACCOUNT_DETAIL = re.compile(
    r"name on their account is (?!\{|\[)\S|Display Name: *(?!\{|\[)\S|X User Handle: *(?!\{|\[)\S|"
    r"Preferred name: *(?!\{|\[)\S|Daniel Rosehill|\bDaniel's\b",
)

# One collector's identity appears in otherwise generic captures, so it is replaced with neutral
# values. The longest forms come first.
IDENTITY = [
    (re.compile(r"asgeirtj@[a-z0-9-]+(?:\.[a-z0-9-]+)+", re.I), "user@example.com"),
    (re.compile(r"[ÁA]sgeir Thor Johnson|[ÁA]sgeir Thor"), "User"),
    (re.compile(r"asgeirtj", re.I), "user"),
    (re.compile(r"[ÁA]sgeir"), "User"),
]

# Captures whose content is the collector's own session rather than a vendor's instructions.
ALWAYS_HOLD = re.compile(
    r"OpenAI/gpt-5\.5-instant\.md|muse-agent/USER\.md|claude-cowork/|claude-projects-|"
    r"tool-advanced-memory"
)

# Files that look like prompts but are only templates around a user's question.
NOT_PROMPTS = re.compile(r"DeepSeek_20250514\.md$")

# Prompts that the local collections lack, published by the vendor or captured elsewhere.
EXTRAS = [
    {
        "id": "deepseek/deepseek-v3-0324-app",
        "title": "DeepSeek V3-0324 app",
        "date": "2025-03",
        "origin": "published",
        "url": "https://huggingface.co/deepseek-ai/DeepSeek-V3-0324/raw/main/README.md",
        "pattern": r"### System Prompt.*?```\n(.*?)\n```",
    },
    {
        "id": "deepseek/deepseek-r1-0528-app",
        "title": "DeepSeek R1-0528 app",
        "date": "2025-05",
        "origin": "published",
        "url": "https://huggingface.co/deepseek-ai/DeepSeek-R1-0528/raw/main/README.md",
        "pattern": r"### System Prompt.*?```\n(.*?)\n```",
    },
    {
        "id": "deepseek/deepseek-r1-web-search-en",
        "title": "DeepSeek R1 web search (English)",
        "date": "2025-01",
        "origin": "published",
        "url": "https://raw.githubusercontent.com/deepseek-ai/DeepSeek-R1/main/README.md",
        "pattern": r"search_answer_en_template = \\\n'''(.*?)'''",
    },
    {
        "id": "deepseek/deepseek-r1-web-search-zh",
        "title": "DeepSeek R1 web search (Chinese)",
        "date": "2025-01",
        "origin": "published",
        "url": "https://raw.githubusercontent.com/deepseek-ai/DeepSeek-R1/main/README.md",
        "pattern": r"search_answer_zh_template = \\\n'''(.*?)'''",
    },
    {
        "id": "deepseek/deepseek-v4-flash",
        "title": "DeepSeek V4-Flash, unverified capture",
        "date": "2026-06",
        "origin": "captured",
        "url": "https://raw.githubusercontent.com/tenth452/DeepSeek-V4-Flash-system-prompt/main/prompt_original_zh.txt",
    },
    {
        "id": "deepseek/deepseek-v4-flash-en",
        "title": "DeepSeek V4-Flash, unverified capture (English translation)",
        "date": "2026-06",
        "origin": "captured",
        "url": "https://raw.githubusercontent.com/tenth452/DeepSeek-V4-Flash-system-prompt/main/prompt_en.txt",
    },
]


def fingerprint(text):
    return hashlib.sha1(re.sub(r"\s+", "", text).lower().encode()).hexdigest()


def slug(text):
    cleaned = re.sub(r"[^\w.+-]+", "-", text.strip().lower(), flags=re.UNICODE).strip("-._")
    if len(cleaned) <= 80:
        return cleaned or "prompt"
    return f"{cleaned[:70].rstrip('-._')}-{hashlib.sha1(cleaned.encode()).hexdigest()[:8]}"


def title_from(name):
    words = re.sub(r"[_]+", " ", name).strip()
    return words[:1].upper() + words[1:]


MONTHS = "jan feb mar apr may jun jul aug sep oct nov dec".split()


def date_from(name):
    if found := re.search(r"(20[23]\d)[-_]?(0[1-9]|1[0-2])[-_]?([0-3]\d)", name):
        return f"{found[1]}-{found[2]}"
    if found := re.search(r"(0[1-9]|1[0-2])([0-3]\d)(20[23]\d)", name):
        return f"{found[3]}-{found[1]}"
    if found := re.search(r"([A-Za-z]{3})[a-z]*[-_ ]\d{1,2}[-_ ](20)?(2\d)", name):
        if found[1].lower() in MONTHS:
            return f"20{found[3]}-{MONTHS.index(found[1].lower()) + 1:02}"
    return ""


def largest_fence(text):
    blocks = re.findall(r"^```[^\n]*\n(.*?)^```\s*$", text, re.MULTILINE | re.DOTALL)
    return max(blocks, key=len).strip() if blocks else None


def extract_gpt(text):
    """Custom GPT dumps: metadata lines, then `GPT instructions:` and the prompt."""
    title = re.search(r"^GPT Title:\s*(.+)$", text, re.MULTILINE)
    parts = re.split(r"^GPT [Ii]nstructions:\s*$", text, maxsplit=1, flags=re.MULTILINE)
    if len(parts) < 2:
        return None, None
    body = re.split(r"^GPT (?:Kb Files List|Protected|Extras):", parts[1], flags=re.MULTILINE)[0]
    return vendor.clean(body), title[1].strip() if title else None


def extract_blackfriday(text):
    title = re.search(r"^#\s+(.+?)(?:\s*\|\s*\[Start Chat\].*)?$", text, re.MULTILINE)
    found = re.search(r"^# Prompt\s*$(.*?)(?=^## (?:Welcome Message|Conversation)|\Z)", text,
                      re.MULTILINE | re.DOTALL)
    if not found:
        return None, None
    return vendor.clean(found[1]), title[1].strip() if title else None


def extract_named_fence(text):
    title = re.search(r"^#+\s*(?:GPT名称[:：])?\s*(.+?)(?:\s*\[Start Chat\].*)?$", text, re.MULTILINE)
    return largest_fence(text), title[1].strip() if title else None


def extract_leaked_gpt(text):
    title = re.search(r"^#\s+(.+?)(?:\s*\[Start Chat\].*)?$", text, re.MULTILINE)
    found = re.search(r"^\*\*(?:Prompt|Instructions?|System Prompt):?\*\*:?\s*$(.*)", text,
                      re.MULTILINE | re.DOTALL)
    body = found[1] if found else (largest_fence(text) or "")
    body = re.split(r"^\*\*(?:Conversation Starters?|Knowledge|Capabilities):?\*\*", body,
                    flags=re.MULTILINE)[0]
    return vendor.clean(body) if body.strip() else None, title[1].strip() if title else None


def strip_capture_header(text):
    """Removes a contributor's notes from the top of a wire capture."""
    found = re.match(r"#[^\n]*\n+- \*\*Contributed by\*\*.*?^---\s*$", text, re.MULTILINE | re.DOTALL)
    text = text[found.end() :] if found else text
    return re.sub(r"\A\s*x-anthropic-billing-header:[^\n]*\n", "", text)


def extract_plain(text):
    return vendor.clean(strip_capture_header(text.replace("\r\n", "\n"))), None


# Directory trees to import, in priority order: the first copy of a duplicate is the one kept.
TREES = [
    ("system_prompts_leaks", "system_prompts_leaks", True, extract_plain),
    ("TheBigPromptLibrary/SystemPrompts", "thebigpromptlibrary", True, extract_plain),
    ("chatgpt_system_prompt/prompts/official-product", "chatgpt-system-prompt", True, extract_plain),
    ("CL4R1T4S", "cl4r1t4s", True, extract_plain),
    ("system-prompts-and-models-of-ai-tools", "ai-tools", True, extract_plain),
    ("awesome-system-prompts/LLMs", "awesome-system-prompts", True, extract_plain),
    ("awesome-system-prompts/Agents", "awesome-system-prompts", False, extract_plain),
    ("chatgpt_system_prompt/prompts/opensource-prj", "open-source-projects", False, extract_plain),
    ("fabric/data/patterns", "fabric", False, extract_plain),
    ("TheBigPromptLibrary/CustomInstructions", "custom-gpts", False, extract_gpt),
    ("chatgpt_system_prompt/prompts/gpts", "custom-gpts", False, extract_gpt),
    ("Leaked-GPTs/gpts", "leaked-gpts", False, extract_leaked_gpt),
    ("GPTS-Prompt-Collection", "gpts-collection", False, extract_named_fence),
    ("BlackFriday-GPTs-Prompts/gpts", "blackfriday-gpts", False, extract_blackfriday),
    ("LangGPT/Prompts", "langgpt", False, extract_plain),
    ("Awesome-Prompts/元提示词", "awesome-prompts", False, extract_plain),
    ("Awesome-Prompts/写作辅助", "awesome-prompts", False, extract_plain),
    ("Awesome-Prompts/实用工具", "awesome-prompts", False, extract_plain),
    ("hf-datasets/General-Purpose-System-Prompts", "hf-general-purpose", False, extract_plain),
    ("hf-datasets/Prompt-Eng-System-Prompts", "hf-prompt-engineering", False, extract_plain),
    ("hypercode/prompts", "hypercode", False, extract_plain),
    ("pikes-prompts", "pikes-prompts", False, extract_plain),
]
SUFFIXES = {".md", ".txt", ".mkd", ".xml", ".prompt"}


class Pack:
    def __init__(self, out):
        self.out = out
        self.entries = []
        self.report = []
        self.seen = {}
        self.ids = set()

    def skip(self, source, decision, reason):
        self.report.append((decision, reason, source))

    def unsafe(self, text, title, source):
        """Screens collections with no editor. Vendor captures quote these words in their rules."""
        haystack = f"{title}\n{source}\n{text}"
        for pattern, reason in ((JAILBREAK, "jailbreak"), (SEXUAL, "sexual"), (HARMFUL, "harmful")):
            if found := pattern.search(haystack):
                return f"{reason}: {found.group()[:40]}"
        return None

    def personal(self, text, strict):
        if found := vendor.DENYLIST.search(text):
            return f"known collector: {found.group()}"
        if found := SECRET.search(text):
            return "credential"
        if found := ACCOUNT_DETAIL.search(text):
            return f"account detail: {found.group()[:40]}"
        for found in EMAIL.finditer(text):
            if not SAFE_EMAIL_DOMAIN.search(found.group().split("@")[1]):
                return "email address"
        for found in HOME_DIR.finditer(text):
            name = found[1].rstrip(".").lower()
            if name not in GENERIC_HOMES and not name.startswith("."):
                return f"home directory: {found[1]}"
        # Vendor prompts list public helplines.
        if strict and PHONE.search(text):
            return "phone number"
        return None

    def add(self, prompt_id, title, date, origin, group, source, text, screen):
        if screen and (reason := self.unsafe(text, title, source)):
            return self.skip(source, "excluded", reason)
        if ALWAYS_HOLD.search(source):
            return self.skip(source, "held", "the collector's own session")
        for pattern, neutral in IDENTITY:
            text = pattern.sub(neutral, text)
        if snapshot := vendor.MEMORY_SNAPSHOT.search(text):
            text = text[: snapshot.start()]
        text = vendor.PERSONAL_LINE.sub("", text).strip() + "\n"
        if len(text.encode()) < MIN_BYTES:
            return self.skip(source, "skipped", "too short")
        if text.lstrip()[:1] in "{[" and re.match(r'\s*[{\[]\s*"', text):
            return self.skip(source, "skipped", "tool schema or data")
        if reason := self.personal(text, screen):
            return self.skip(source, "held", reason)
        mark = fingerprint(text)
        if mark in self.seen:
            return self.skip(source, "skipped", f"duplicate of {self.seen[mark]}")
        self.seen[mark] = prompt_id

        unique, count = prompt_id, 2
        while unique.lower() in self.ids:
            unique, count = f"{prompt_id}-{count}", count + 1
        self.ids.add(unique.lower())
        target = self.out / "prompts" / f"{unique}.md"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
        size = len(text.encode())
        clean_title = re.sub(r"\s+", " ", title or unique.rsplit("/", 1)[-1]).strip()[:120]
        self.entries.append((unique, clean_title, group, date, origin, size, source))
        self.report.append(("included", unique, source))


def tree_files(base):
    for directory, folders, names in os.walk(base):
        folders[:] = sorted(name for name in folders if name != ".git")
        for name in sorted(names):
            yield Path(directory) / name


def vendor_of(relative):
    return VENDOR_FOLDERS.get(relative.parts[0].lower()) if len(relative.parts) > 1 else None


def import_tree(pack, library, tree, label, by_vendor, extract, jailbreak_slugs, built_in):
    base = library / tree
    if not base.is_dir():
        return
    for path in tree_files(base):
        relative = path.relative_to(base)
        source = f"{tree}/{relative}"
        if path.suffix.lower() not in SUFFIXES:
            continue
        if source in built_in:
            pack.skip(source, "skipped", "already built in")
            continue
        if NOT_PROMPTS.search(source):
            pack.skip(source, "skipped", "not a prompt")
            continue
        if label == "fabric" and path.name != "system.md":
            continue
        if NOT_A_PROMPT_NAME.match(path.stem) or NOT_A_PROMPT_PATH.search(f"/{relative}"):
            pack.skip(source, "skipped", "not a prompt")
            continue
        if COMPANION_PATH.search(source):
            pack.skip(source, "excluded", "sexual: companion persona")
            continue
        if label == "blackfriday-gpts" and path.stem in jailbreak_slugs:
            pack.skip(source, "excluded", "jailbreak: listed in the collection's jailbreak index")
            continue
        size = path.stat().st_size
        if not MIN_BYTES <= size <= MAX_BYTES:
            pack.skip(source, "skipped", "outside the size limits")
            continue
        raw = path.read_text(encoding="utf-8", errors="replace")
        text, found_title = extract(raw)
        if not text or not text.strip():
            pack.skip(source, "skipped", "no prompt found in the file")
            continue

        stem = path.parent.name if label == "fabric" else path.stem
        family = vendor_of(relative) if by_vendor else None
        if family:
            prompt_id = f"{family}/{slug(stem)}"
            group = VENDOR_NAMES[family]
            origin = "published" if "/official/" in source else "captured"
        else:
            prompt_id = f"uncategorized/{label}/{slug(stem)}"
            group = UNCATEGORIZED
            origin = "community"
        title = found_title or title_from(stem)
        pack.add(prompt_id, title, date_from(stem), origin, group, source, text, not family)


def import_records(pack, label, source, records):
    for number, (title, text) in enumerate(records):
        text = (text or "").strip()
        title = (title or f"Prompt {number + 1}").strip()
        reference = f"{source}#{number + 1}"
        if not text:
            pack.skip(reference, "skipped", "no prompt found in the file")
            continue
        prompt_id = f"uncategorized/{label}/{slug(title)}"
        pack.add(prompt_id, title, "", "community", UNCATEGORIZED, reference, text + "\n", True)


def read_csv(path, title_field, text_field, keep=lambda row: True):
    csv.field_size_limit(sys.maxsize)
    with open(path, encoding="utf-8", errors="replace", newline="") as handle:
        return [(r.get(title_field), r.get(text_field)) for r in csv.DictReader(handle) if keep(r)]


def import_datasets(pack, library):
    awesome = library / "awesome-chatgpt-prompts/prompts.csv"
    if awesome.exists():
        import_records(pack, "awesome-chatgpt-prompts", str(awesome.relative_to(library)),
                       read_csv(awesome, "act", "prompt"))
    chat = library / "prompts.chat/prompts.csv"
    if chat.exists():
        records = read_csv(chat, "act", "prompt", lambda row: row.get("type", "TEXT") == "TEXT")
        import_records(pack, "prompts-chat", str(chat.relative_to(library)), records)

    folder = library / "hf-datasets/System-Prompt-Library/system-prompts/json"
    if folder.is_dir():
        records = []
        for path in sorted(folder.glob("*.json")):
            try:
                row = json.loads(path.read_text(encoding="utf-8", errors="replace"))
            except json.JSONDecodeError:
                continue
            records.append((row.get("agentname"), row.get("systemprompt")))
        import_records(pack, "hf-system-prompt-library", str(folder.relative_to(library)), records)

    lines = library / "hf-datasets/teilomillet-system_prompt/final_system_prompts_69.jsonl"
    if lines.exists():
        records = []
        for line in lines.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            text = next((row[k] for k in ("system_prompt", "prompt", "text") if row.get(k)), None)
            records.append((row.get("name") or row.get("title"), text))
        import_records(pack, "hf-teilomillet", str(lines.relative_to(library)), records)


def fetch(url, cache, offline):
    target = cache / hashlib.sha1(url.encode()).hexdigest()
    if not target.exists() and not offline:
        request = urllib.request.Request(url, headers={"User-Agent": "sparkle-prompt-pack"})
        with urllib.request.urlopen(request, timeout=30) as response:
            data = response.read(MAX_BYTES)
        cache.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    return target.read_text(encoding="utf-8", errors="replace") if target.exists() else None


def import_extras(pack, cache, offline):
    for extra in EXTRAS:
        try:
            text = fetch(extra["url"], cache, offline)
        except OSError as error:
            pack.skip(extra["url"], "skipped", f"download failed: {error}")
            continue
        if text is None:
            pack.skip(extra["url"], "skipped", "not downloaded")
            continue
        if pattern := extra.get("pattern"):
            found = re.search(pattern, text, re.DOTALL)
            if not found:
                pack.skip(extra["url"], "skipped", "prompt not found on the page")
                continue
            text = found[1]
        group = VENDOR_NAMES[extra["id"].split("/")[0]]
        pack.add(extra["id"], extra["title"], extra["date"], extra["origin"], group, extra["url"],
                 text.strip() + "\n", False)


def default_out():
    home = os.environ.get("SPARKLE_HOME")
    if home:
        return Path(home) / "prompt-pack"
    data = os.environ.get("XDG_DATA_HOME") or str(Path.home() / ".local/share")
    return Path(data) / "sparkle" / "prompt-pack"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--library", type=Path, default=Path.home() / "Documents/prompt-libraries")
    parser.add_argument("--out", type=Path, default=default_out())
    parser.add_argument("--offline", action="store_true", help="use only downloads already cached")
    args = parser.parse_args()

    building = args.out.with_name(args.out.name + ".building")
    shutil.rmtree(building, ignore_errors=True)
    building.mkdir(parents=True)
    pack = Pack(building)

    # Anything already built into sparkle does not need a second row.
    for path in BUILT_IN.rglob("*.md"):
        if path.name != "README.md":
            pack.seen[fingerprint(path.read_text(encoding="utf-8"))] = "built-in"
    built_in = set()
    for line in (ROOT / "scripts/prompt-manifest.tsv").read_text(encoding="utf-8").splitlines():
        if line.strip() and not line.startswith("#"):
            prompt_id, *_, source = line.split("\t")
            prefix, relative = source.split(":", 1)
            built_in.add(f"{vendor.SOURCES[prefix]}/{relative}")
            pack.ids.add(prompt_id.lower())
    for name, reason in EXCLUDED_COLLECTIONS.items():
        pack.skip(name, "excluded", reason)

    index = args.library / "BlackFriday-GPTs-Prompts/Jailbreaks.md"
    listed = index.read_text(encoding="utf-8", errors="replace") if index.exists() else ""
    jailbreak_slugs = set(re.findall(r"gpts/([^)\s/]+)\.md", listed))

    import_extras(pack, args.out.with_name("prompt-pack-downloads"), args.offline)
    for tree, label, by_vendor, extract in TREES:
        import_tree(pack, args.library, tree, label, by_vendor, extract, jailbreak_slugs, built_in)
    import_datasets(pack, args.library)

    with open(building / "index.tsv", "w", encoding="utf-8", newline="\n") as handle:
        handle.write("# sparkle prompt pack 1\n")
        for entry in sorted(pack.entries, key=lambda row: row[0].lower()):
            fields = [re.sub(r"[\t\r\n]+", " ", str(field)) for field in entry]
            handle.write("\t".join(fields) + "\n")
    with open(building / "REPORT.tsv", "w", encoding="utf-8", newline="\n") as handle:
        handle.write("decision\treason\tsource\n")
        for row in pack.report:
            handle.write("\t".join(re.sub(r"[\t\r\n]+", " ", str(field)) for field in row) + "\n")

    shutil.rmtree(args.out, ignore_errors=True)
    building.rename(args.out)

    decisions = Counter(row[0] for row in pack.report)
    groups = Counter(entry[2] for entry in pack.entries)
    total = sum(entry[5] for entry in pack.entries)
    print(f"pack: {args.out}")
    print(f"included {decisions['included']} prompts, {total / 1048576:.1f} MB")
    print(f"held for review {decisions['held']}, excluded {decisions['excluded']}, "
          f"skipped {decisions['skipped']}")
    for group, count in groups.most_common():
        print(f"  {group:<16}{count}")


if __name__ == "__main__":
    main()
