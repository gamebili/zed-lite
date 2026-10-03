"""Check settings labels, descriptions and built-in translated UI strings."""

import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT / "crates/settings_ui/src"
CATALOG = ROOT / "crates/i18n/locales/settings.zh-CN.json"
STRING = r'"((?:\\.|[^"\\])*)"'


def decode(value):
    return json.loads('"' + value + '"')


def required_keys():
    source = (SOURCE / "page_data.rs").read_text(encoding="utf-8")
    pattern = re.compile(
        r'\b(?:title|description|button_text):\s*(?:Some\(\s*)?' + STRING
        + r'|SectionHeader\s*\(\s*' + STRING
    )
    keys = {decode(match.group(1) or match.group(2)) for match in pattern.finditer(source)}
    for path in SOURCE.rglob("*.rs"):
        content = path.read_text(encoding="utf-8")
        for match in re.finditer(r'\b(?:tr|i18n::text)\(\s*' + STRING, content):
            keys.add(decode(match.group(1)))
    return keys


def dropdown_labels():
    content = (SOURCE / "settings_ui.rs").read_text(encoding="utf-8")
    names = set(re.findall(r'add_basic_renderer::<settings::(\w+)>\(render_dropdown\)', content))
    declarations = {}
    for path in (ROOT / "crates/settings_content/src").rglob("*.rs"):
        source = path.read_text(encoding="utf-8")
        for match in re.finditer(r'pub enum (\w+)\s*\{(.*?)\n\}', source, re.DOTALL):
            declarations[match.group(1)] = match.group(2)
    labels = set()
    for name in names:
        if name == "UiLanguage":
            labels.update({"Follow System", "English", "简体中文", "繁體中文"})
            continue
        body = declarations.get(name.removesuffix("Discriminants"))
        if body is None:
            raise ValueError(f"Enum not found: {name}")
        attributes = []
        for line in body.splitlines():
            if line.startswith("    #["):
                attributes.append(line)
            variant = re.match(r'^    (\w+)(?:\s*[,({=])', line)
            if variant:
                serialized = re.findall(r'strum\(serialize\s*=\s*"([^"]+)"', " ".join(attributes))
                raw = serialized[0] if serialized else variant.group(1)
                label = title_case(raw)
                labels.add(raw if name in {"BaseKeymapContent", "LineEndingSetting", "CliDefaultOpenBehavior", "DefaultOpenBehavior"} else label)
                attributes.clear()
    return labels


def title_case(value):
    words = []
    for word in re.split(r'[^\w]|_', value):
        start = 0
        mode = "boundary"
        for index, char in enumerate(word):
            if index + 1 == len(word):
                words.append(word[start:].capitalize())
                break
            following = word[index + 1]
            next_mode = "lower" if char.islower() else "upper" if char.isupper() else mode
            if next_mode == "lower" and following.isupper():
                words.append(word[start:index + 1].capitalize())
                start = index + 1
                mode = "boundary"
            elif mode == "upper" and char.isupper() and following.islower():
                words.append(word[start:index].capitalize())
                start = index
                mode = "boundary"
            else:
                mode = next_mode
    return " ".join(words)


if __name__ == "__main__":
    catalog = json.loads(CATALOG.read_text(encoding="utf-8"))
    required = required_keys() | dropdown_labels()
    missing = sorted(required - catalog.keys())
    empty = sorted(key for key, value in catalog.items() if not isinstance(value, str) or not value)
    print(f"{len(required)} required keys, {len(catalog)} catalog entries, {len(missing)} missing, {len(empty)} empty")
    for key in missing:
        print(json.dumps(key, ensure_ascii=True))
    if empty:
        print("Empty translations: " + json.dumps(empty, ensure_ascii=True))
    raise SystemExit(bool(missing or empty))
