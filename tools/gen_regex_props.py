#!/usr/bin/env python3
"""Generates src/regex_props_tables.rs: Unicode properties for regular
expression property escapes (General_Category, Script,
Script_Extensions and the binary properties of ECMA-262 table 67).

    python3 tools/gen_regex_props.py ~/src/ucd

Data: Unicode License v3 (https://www.unicode.org/license.txt).
"""
import os
import sys
from collections import defaultdict

ucd = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/src/ucd")


def merge(rs):
    rs = sorted(rs)
    out = []
    for a, b in rs:
        if out and a <= out[-1][1] + 1:
            out[-1] = (out[-1][0], max(out[-1][1], b))
        else:
            out.append((a, b))
    return out


def read_prop_file(fname):
    """field1 -> ranges."""
    props = defaultdict(list)
    for line in open(os.path.join(ucd, fname), encoding="utf-8"):
        line = line.split("#")[0].strip()
        if not line:
            continue
        cps, prop = [x.strip() for x in line.split(";")[:2]]
        a, _, b = cps.partition("..")
        props[prop].append((int(a, 16), int(b or a, 16)))
    return {k: merge(v) for k, v in props.items()}


binary_names = """ASCII ASCII_Hex_Digit Alphabetic Any Assigned Bidi_Control Bidi_Mirrored
Case_Ignorable Cased Changes_When_Casefolded Changes_When_Casemapped Changes_When_Lowercased
Changes_When_NFKC_Casefolded Changes_When_Titlecased Changes_When_Uppercased Dash
Default_Ignorable_Code_Point Deprecated Diacritic Emoji Emoji_Component Emoji_Modifier
Emoji_Modifier_Base Emoji_Presentation Extended_Pictographic Extender Grapheme_Base
Grapheme_Extend Hex_Digit IDS_Binary_Operator IDS_Trinary_Operator ID_Continue ID_Start
Ideographic Join_Control Logical_Order_Exception Lowercase Math Noncharacter_Code_Point
Pattern_Syntax Pattern_White_Space Quotation_Mark Radical Regional_Indicator Sentence_Terminal
Soft_Dotted Terminal_Punctuation Unified_Ideograph Uppercase Variation_Selector White_Space
XID_Continue XID_Start""".split()

sources = {}
for f in ["PropList.txt", "DerivedCoreProperties.txt", "emoji-data.txt", "DerivedBinaryProperties.txt", "DerivedNormalizationProps.txt"]:
    for k, v in read_prop_file(f).items():
        sources.setdefault(k, v)

gc = read_prop_file("DerivedGeneralCategory.txt")
scripts = read_prop_file("Scripts.txt")

# Property value aliases.
aliases = defaultdict(list)   # (prop, canonical) -> [aliases]
prop_aliases = {}
for line in open(os.path.join(ucd, "PropertyValueAliases.txt"), encoding="utf-8"):
    line = line.split("#")[0].strip()
    if not line:
        continue
    f = [x.strip() for x in line.split(";")]
    if f[0] in ("gc", "sc"):
        short, long_ = f[1], f[2]
        names = [short, long_] + f[3:]
        aliases[(f[0], long_)] = [n for n in names if n]
for line in open(os.path.join(ucd, "PropertyAliases.txt"), encoding="utf-8"):
    line = line.split("#")[0].strip()
    if not line:
        continue
    f = [x.strip() for x in line.split(";")]
    prop_aliases[f[1]] = f

# Script_Extensions: code points -> list of short script names.
scx = defaultdict(list)
short_to_long = {}
for (p, long_), names in aliases.items():
    if p == "sc":
        short_to_long[names[0]] = long_
for line in open(os.path.join(ucd, "ScriptExtensions.txt"), encoding="utf-8"):
    line = line.split("#")[0].strip()
    if not line:
        continue
    cps, sc = [x.strip() for x in line.split(";")[:2]]
    a, _, b = cps.partition("..")
    for s in sc.split():
        scx[short_to_long.get(s, s)].append((int(a, 16), int(b or a, 16)))
# scx(c) = sc(c) when not listed.
listed = merge([r for v in scx.values() for r in v])


def subtract(a, b):
    out = []
    for x, y in a:
        cur = x
        for p, q in b:
            if q < cur or p > y:
                continue
            if p > cur:
                out.append((cur, p - 1))
            cur = max(cur, q + 1)
            if cur > y:
                break
        if cur <= y:
            out.append((cur, y))
    return out


tables = {}
for n in binary_names:
    if n == "Any":
        continue
    if n == "ASCII":
        tables["B_ASCII"] = [(0, 0x7F)]
        continue
    if n == "Assigned":
        tables["B_Assigned"] = subtract([(0, 0x10FFFF)], gc["Cn"])
        continue
    tables["B_" + n] = sources[n]
for long_ in set(k[1] for k in aliases if k[0] == "gc"):
    short = aliases[("gc", long_)][0]
    if short in gc:
        tables["G_" + short] = gc[short]
# Group categories.
groups = {"L": ["Lu", "Ll", "Lt", "Lm", "Lo"], "LC": ["Lu", "Ll", "Lt"], "M": ["Mn", "Mc", "Me"], "N": ["Nd", "Nl", "No"],
          "P": ["Pc", "Pd", "Ps", "Pe", "Pi", "Pf", "Po"], "S": ["Sm", "Sc", "Sk", "So"], "Z": ["Zs", "Zl", "Zp"],
          "C": ["Cc", "Cf", "Cs", "Co", "Cn"]}
for g, parts in groups.items():
    tables["G_" + g] = merge([r for p in parts for r in gc[p]])
for long_ in scripts:
    tables["S_" + long_] = scripts[long_]
    tables["X_" + long_] = merge(subtract(scripts[long_], listed) + scx.get(long_, []))
for long_ in scx:
    if "X_" + long_ not in tables:
        tables["X_" + long_] = merge(scx[long_])

with open("src/regex_props_tables.rs", "w", encoding="utf-8") as f:
    f.write("//! Generated by tools/gen_regex_props.py from the Unicode Character Database.\n")
    f.write("//! Data: Unicode License v3 (https://www.unicode.org/license.txt).\n\n")
    f.write("pub static TABLES: &[(&str, &[(u32, u32)])] = &[\n")
    for name in sorted(tables):
        rs = tables[name]
        f.write(f"    (\"{name}\", &[")
        f.write(", ".join(f"(0x{a:X}, 0x{b:X})" for a, b in rs))
        f.write("]),\n")
    f.write("];\n\n")
    # Name aliases: binary properties, gc values, sc values.
    f.write("pub static BINARY_ALIASES: &[(&str, &str)] = &[\n")
    for n in binary_names:
        for a in prop_aliases.get(n, [n]):
            f.write(f"    (\"{a}\", \"{n}\"),\n")
    f.write("];\n\n")
    f.write("pub static GC_ALIASES: &[(&str, &str)] = &[\n")
    for (p, long_), names in sorted(aliases.items()):
        if p == "gc":
            for a in names:
                f.write(f"    (\"{a}\", \"{names[0]}\"),\n")
    f.write("];\n\n")
    f.write("pub static SC_ALIASES: &[(&str, &str)] = &[\n")
    for (p, long_), names in sorted(aliases.items()):
        if p == "sc":
            for a in names:
                f.write(f"    (\"{a}\", \"{long_}\"),\n")
    f.write("];\n")
print("tables:", len(tables))
