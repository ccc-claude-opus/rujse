//! The YAML front matter of a test262 test (`/*--- … ---*/`): only the
//! keys the runner needs, parsed from the small YAML subset test262 uses.

#[derive(Default, Debug)]
pub struct Flags {
    pub only_strict: bool,
    pub no_strict: bool,
    pub module: bool,
    pub raw: bool,
    pub is_async: bool,
    pub can_block_is_false: bool,
}

#[derive(Debug)]
pub struct Negative {
    pub phase: String,
    pub kind: String,
}

#[derive(Default, Debug)]
pub struct Meta {
    pub flags: Flags,
    pub includes: Vec<String>,
    pub features: Vec<String>,
    pub negative: Option<Negative>,
}

fn inline_list(v: &str) -> Vec<String> {
    v.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

impl Meta {
    pub fn parse(src: &str) -> Meta {
        let mut m = Meta::default();
        let Some(start) = src.find("/*---") else { return m };
        let Some(end) = src[start..].find("---*/") else { return m };
        let yaml = &src[start + 5..start + end];
        let mut key = String::new();
        let mut neg_phase = String::new();
        let mut neg_type = String::new();
        let mut flags = Vec::new();
        for line in yaml.lines() {
            let indented = line.starts_with(' ') || line.starts_with('\t');
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if !indented {
                if let Some((k, v)) = t.split_once(':') {
                    key = k.trim().to_string();
                    let v = v.trim();
                    match key.as_str() {
                        "flags" if v.starts_with('[') => flags = inline_list(v),
                        "includes" if v.starts_with('[') => m.includes = inline_list(v),
                        "features" if v.starts_with('[') => m.features = inline_list(v),
                        _ => {}
                    }
                }
                continue;
            }
            // Indented: list items or the `negative` mapping.
            if let Some(item) = t.strip_prefix("- ") {
                let item = item.trim().to_string();
                match key.as_str() {
                    "includes" => m.includes.push(item),
                    "features" => m.features.push(item),
                    "flags" => flags.push(item),
                    _ => {}
                }
            } else if key == "negative" {
                if let Some((k, v)) = t.split_once(':') {
                    match k.trim() {
                        "phase" => neg_phase = v.trim().to_string(),
                        "type" => neg_type = v.trim().to_string(),
                        _ => {}
                    }
                }
            }
        }
        for f in &flags {
            match f.as_str() {
                "onlyStrict" => m.flags.only_strict = true,
                "noStrict" => m.flags.no_strict = true,
                "module" => m.flags.module = true,
                "raw" => m.flags.raw = true,
                "async" => m.flags.is_async = true,
                "CanBlockIsFalse" => m.flags.can_block_is_false = true,
                _ => {}
            }
        }
        if !neg_phase.is_empty() {
            m.negative = Some(Negative { phase: neg_phase, kind: neg_type });
        }
        m
    }
}
