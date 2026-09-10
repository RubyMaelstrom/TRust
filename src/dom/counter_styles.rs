//! CSS Counter Styles 3: descriptor validation and bounded representation
//! algorithms. CSSWG snapshot 81c27f686901 (2026-09-06), §§2–7.
use super::*;
use unicode_segmentation::UnicodeSegmentation;

const MAX_SYMBOLS: usize = 1024;
const MAX_BYTES: usize = 4096;
pub(super) type Styles = FxHashMap<String, (u64, CounterStyle)>;
#[derive(Clone, Debug, Default)]
pub(super) struct CounterStyle {
    descriptors: FxHashMap<String, String>,
    algorithm: Option<Complex>,
}
#[derive(Clone, Copy, Debug)]
enum Complex {
    Chinese { formal: bool, traditional: bool },
    Ethiopic,
}

pub(super) fn identifier(raw: &str) -> Option<String> {
    let mut chars = raw.chars().peekable();
    let mut decoded = String::new();
    let mut first = true;
    while let Some(c) = chars.next() {
        if c == '\\' {
            let mut hex = String::new();
            while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
                hex.push(chars.next()?);
            }
            let c = if hex.is_empty() {
                let c = chars.next()?;
                if matches!(c, '\n' | '\r' | '\x0c') {
                    return None;
                }
                c
            } else {
                if chars
                    .peek()
                    .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c'))
                {
                    chars.next();
                }
                char::from_u32(u32::from_str_radix(&hex, 16).ok()?)
                    .filter(|c| *c != '\0')
                    .unwrap_or('\u{fffd}')
            };
            decoded.push(c);
        } else if c == '-'
            || c == '_'
            || c.is_ascii_alphabetic()
            || !c.is_ascii()
            || (!first && c.is_ascii_digit())
        {
            if first && c == '-' && chars.peek().is_some_and(char::is_ascii_digit) {
                return None;
            }
            decoded.push(c);
        } else {
            return None;
        }
        first = false;
    }
    (!decoded.is_empty() && decoded != "-").then_some(decoded)
}
fn symbol(v: &str) -> Option<String> {
    unquote_css(v).or_else(|| identifier(v))
}
fn symbols(v: &str) -> Option<Vec<String>> {
    split_top_level_ws(v).into_iter().map(symbol).collect()
}
pub(super) fn valid_name(v: &str) -> bool {
    identifier(v).is_some_and(|v| {
        wide_keyword(&v).is_none()
            && !v.eq_ignore_ascii_case("none")
            && !v.eq_ignore_ascii_case("default")
    })
}
pub(super) fn descriptor_valid(name: &str, v: &str) -> bool {
    let tokens = split_top_level_ws(v);
    if !cssom::valid_value(v) {
        return false;
    }
    match name {
        "system" => match tokens.as_slice() {
            ["cyclic" | "numeric" | "alphabetic" | "symbolic" | "additive" | "fixed"] => true,
            ["fixed", n] => n.parse::<i64>().is_ok(),
            ["extends", n] => valid_name(n),
            _ => false,
        },
        "symbols" => !tokens.is_empty() && symbols(v).is_some(),
        "additive-symbols" => additive(v).is_some(),
        "negative" => (1..=2).contains(&tokens.len()) && symbols(v).is_some(),
        "prefix" | "suffix" => tokens.len() == 1 && symbol(v).is_some(),
        "pad" => pad(v).is_some(),
        "range" => ranges(v).is_some(),
        "fallback" => valid_name(v),
        "speak-as" => {
            matches!(v, "auto" | "bullets" | "numbers" | "words" | "spell-out") || valid_name(v)
        }
        _ => false,
    }
}
fn additive(v: &str) -> Option<Vec<(u64, String)>> {
    let mut out = Vec::new();
    for tuple in split_top_level(v, ',') {
        let t = split_top_level_ws(tuple);
        let [a, b] = t.as_slice() else {
            return None;
        };
        let (n, s) = if let Ok(n) = a.parse::<u64>() {
            (n, symbol(b)?)
        } else {
            (b.parse().ok()?, symbol(a)?)
        };
        if out.last().is_some_and(|(last, _)| *last <= n) {
            return None;
        }
        out.push((n, s));
    }
    (!out.is_empty()).then_some(out)
}
fn pad(v: &str) -> Option<(usize, String)> {
    let t = split_top_level_ws(v);
    let [a, b] = t.as_slice() else {
        return None;
    };
    if let Ok(n) = a.parse() {
        Some((n, symbol(b)?))
    } else {
        Some((b.parse().ok()?, symbol(a)?))
    }
}
fn ranges(v: &str) -> Option<Vec<(i64, i64)>> {
    if v == "auto" {
        return Some(vec![]);
    }
    split_top_level(v, ',')
        .into_iter()
        .map(|s| {
            let t = split_top_level_ws(s);
            let [a, b] = t.as_slice() else {
                return None;
            };
            let a = if *a == "infinite" {
                i64::MIN
            } else {
                a.parse().ok()?
            };
            let b = if *b == "infinite" {
                i64::MAX
            } else {
                b.parse().ok()?
            };
            (a <= b).then_some((a, b))
        })
        .collect()
}
impl CounterStyle {
    pub(super) fn parse(text: &str) -> Self {
        let mut result = Self::default();
        let text = strip_css_comments(text);
        for decl in split_top_level(&text, ';') {
            let Some((name, value)) = decl.split_once(':') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if descriptor_valid(&name, value) {
                result.descriptors.insert(name, value.into());
            }
        }
        result
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.descriptors.capacity() * std::mem::size_of::<(String, String)>()
            + self
                .descriptors
                .iter()
                .map(|(k, v)| k.capacity() + v.capacity())
                .sum::<usize>()
    }
    fn value(&self, name: &str) -> Option<&str> {
        self.descriptors.get(name).map(String::as_str)
    }
    pub(super) fn valid(&self) -> bool {
        if self.algorithm.is_some() {
            return true;
        }
        let system = self.value("system").unwrap_or("symbolic");
        if system.starts_with("extends ") {
            return self.value("symbols").is_none() && self.value("additive-symbols").is_none();
        }
        if system == "additive" {
            return self.value("additive-symbols").and_then(additive).is_some();
        }
        self.value("symbols").and_then(symbols).is_some_and(|v| {
            v.len()
                >= if matches!(system, "numeric" | "alphabetic") {
                    2
                } else {
                    1
                }
        })
    }
    fn initial(&self, value: i64) -> Option<String> {
        let system = split_top_level_ws(self.value("system").unwrap_or("symbolic"));
        let kind = system[0];
        let uses_negative = matches!(kind, "numeric" | "alphabetic" | "symbolic" | "additive");
        let n = if uses_negative {
            value.unsigned_abs() as i128
        } else {
            value as i128
        };
        let range = ranges(self.value("range").unwrap_or("auto"))?;
        if (!range.is_empty() && !range.iter().any(|(a, b)| *a <= value && value <= *b))
            || (range.is_empty()
                && ((matches!(kind, "alphabetic" | "symbolic") && value < 1)
                    || (kind == "additive" && value < 0)))
        {
            return None;
        }
        let syms = self.value("symbols").and_then(symbols).unwrap_or_default();
        let count = syms.len() as i128;
        let mut result = String::new();
        if let Some(algorithm) = self.algorithm {
            result = algorithm.initial(value.unsigned_abs())?;
        } else {
            match kind {
                "cyclic" if count > 0 => {
                    result = syms[((n - 1).rem_euclid(count)) as usize].clone()
                }
                "fixed" if count > 0 => {
                    let start = system
                        .get(1)
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(1) as i128;
                    result = syms.get(usize::try_from(n - start).ok()?)?.clone();
                }
                "symbolic" if count > 0 && n > 0 => {
                    let copies = usize::try_from((n + count - 1) / count).ok()?;
                    if copies > MAX_SYMBOLS {
                        return None;
                    }
                    let symbol = &syms[((n - 1) % count) as usize];
                    if symbol.len().checked_mul(copies)? > MAX_BYTES {
                        return None;
                    }
                    result = symbol.repeat(copies);
                }
                "numeric" | "alphabetic" if count >= 2 => {
                    if n == 0 && kind == "alphabetic" {
                        return None;
                    }
                    let mut n = n;
                    let mut parts = Vec::new();
                    loop {
                        if kind == "alphabetic" {
                            n -= 1;
                        }
                        parts.push(syms[(n % count) as usize].as_str());
                        n /= count;
                        if n == 0 {
                            break;
                        }
                    }
                    if parts.iter().map(|s| s.len()).sum::<usize>() > MAX_BYTES {
                        return None;
                    }
                    result = parts.into_iter().rev().collect();
                }
                "additive" => {
                    let tuples = additive(self.value("additive-symbols")?)?;
                    let mut n = n as u64;
                    if n == 0 {
                        result = tuples.iter().find(|(n, _)| *n == 0)?.1.clone();
                    } else {
                        let mut total = 0usize;
                        for (weight, sym) in tuples {
                            if weight == 0 {
                                continue;
                            }
                            let copies = usize::try_from(n / weight).ok()?;
                            total = total.checked_add(copies)?;
                            if total > MAX_SYMBOLS
                                || result.len().checked_add(sym.len().checked_mul(copies)?)?
                                    > MAX_BYTES
                            {
                                return None;
                            }
                            result.push_str(&sym.repeat(copies));
                            n %= weight;
                        }
                        if n != 0 {
                            return None;
                        }
                    }
                }
                _ => return None,
            }
        }
        if result.len() > MAX_BYTES {
            return None;
        }
        let negative = symbols(self.value("negative").unwrap_or("\"-\""))?;
        let signs = value < 0 && uses_negative;
        if let Some((width, sym)) = self.value("pad").and_then(pad) {
            if width > MAX_SYMBOLS {
                return None;
            }
            let count = result.graphemes(true).count()
                + if signs {
                    negative.iter().map(|s| s.graphemes(true).count()).sum()
                } else {
                    0
                };
            let copies = width.saturating_sub(count);
            if result.len().checked_add(sym.len().checked_mul(copies)?)? > MAX_BYTES {
                return None;
            }
            result.insert_str(0, &sym.repeat(copies));
        }
        if signs && negative.iter().map(|s| s.len()).sum::<usize>() + result.len() > MAX_BYTES {
            return None;
        }
        if signs {
            result = format!(
                "{}{}{}",
                negative[0],
                result,
                negative.get(1).map_or("", String::as_str)
            );
        }
        (result.chars().count() <= MAX_SYMBOLS).then_some(result)
    }
}
// CSS Counter Styles 3 §§7.1.2.2 and 7.2. These algorithms can be
// inherited with system:extends just like the predefined descriptor styles.
impl Complex {
    fn initial(self, n: u64) -> Option<String> {
        let mut out = String::new();
        match self {
            Self::Chinese {
                formal,
                traditional,
            } => {
                if n > 9999 {
                    return None;
                }
                let digits: Vec<char> = match (formal, traditional) {
                    (false, _) => "零一二三四五六七八九",
                    (true, false) => "零壹贰叁肆伍陆柒捌玖",
                    (true, true) => "零壹貳參肆伍陸柒捌玖",
                }
                .chars()
                .collect();
                let markers: Vec<char> = if formal { "拾佰仟" } else { "十百千" }.chars().collect();
                if n == 0 {
                    return Some(digits[0].to_string());
                }
                let mut zero = false;
                for power in (0u32..4).rev() {
                    let digit = (n / 10u64.pow(power)) % 10;
                    if digit == 0 {
                        zero |= !out.is_empty();
                        continue;
                    }
                    if zero {
                        out.push(digits[0]);
                        zero = false;
                    }
                    if !(power == 1 && !formal && (10..20).contains(&n)) {
                        out.push(digits[digit as usize]);
                    }
                    if power > 0 {
                        out.push(markers[(power - 1) as usize]);
                    }
                }
            }
            Self::Ethiopic => {
                if n == 0 {
                    return None;
                }
                if n == 1 {
                    return Some("፩".into());
                }
                let mut groups = Vec::new();
                let mut n = n;
                while n > 0 {
                    groups.push(n % 100);
                    n /= 100;
                }
                for (i, &group) in groups.iter().enumerate().rev() {
                    if group != 0 && !(group == 1 && (i == groups.len() - 1 || i % 2 == 1)) {
                        if group / 10 > 0 {
                            out.push(char::from_u32(0x1371 + (group / 10) as u32)?);
                        }
                        if group % 10 > 0 {
                            out.push(char::from_u32(0x1368 + (group % 10) as u32)?);
                        }
                    }
                    if i % 2 == 1 && group != 0 {
                        out.push('፻');
                    } else if i % 2 == 0 && i > 0 {
                        out.push('፼');
                    }
                }
            }
        }
        Some(out)
    }
}

fn builtins() -> &'static FxHashMap<String, CounterStyle> {
    static STYLES: std::sync::LazyLock<FxHashMap<String, CounterStyle>> = std::sync::LazyLock::new(
        || {
            let mut out = FxHashMap::default();
            for rule in include_str!("counter_styles.css")
                .split("@counter-style")
                .skip(1)
            {
                if let Some((name, body)) = rule.split_once('{') {
                    out.insert(
                        name.trim().into(),
                        CounterStyle::parse(body.split_once('}').unwrap().0),
                    );
                }
            }
            // CSS Counter Styles 3 §6.3 permits direction-dependent triangle
            // glyphs. Horizontal LTR defaults; the marker caller mirrors closed
            // disclosure in RTL.
            out.get_mut("disclosure-open")
                .unwrap()
                .descriptors
                .insert("symbols".into(), "'▾'".into());
            out.get_mut("disclosure-closed")
                .unwrap()
                .descriptors
                .insert("symbols".into(), "'▸'".into());
            for (name, formal, traditional) in [
                ("simp-chinese-informal", false, false),
                ("simp-chinese-formal", true, false),
                ("trad-chinese-informal", false, true),
                ("trad-chinese-formal", true, true),
                ("cjk-ideographic", false, true),
            ] {
                let mut style = CounterStyle::parse(&format!(
                    "system:numeric;range:-9999 9999;suffix:'、';fallback:cjk-decimal;negative:'{}'",
                    if traditional { "負" } else { "负" }
                ));
                style.algorithm = Some(Complex::Chinese {
                    formal,
                    traditional,
                });
                out.insert(name.into(), style);
            }
            let mut ethiopic = CounterStyle::parse("system:numeric;range:1 infinite;suffix:'/ '");
            ethiopic.algorithm = Some(Complex::Ethiopic);
            out.insert("ethiopic-numeric".into(), ethiopic);
            out
        },
    );
    &STYLES
}
pub(super) fn normalize_name(name: &str) -> String {
    let name = identifier(name).unwrap_or_else(|| name.to_string());
    let lower = name.to_ascii_lowercase();
    if builtins().contains_key(&lower) {
        lower
    } else {
        name
    }
}
fn resolve_style(name: &str, styles: &Styles, _active: &mut Vec<String>) -> Option<CounterStyle> {
    let mut name = normalize_name(name);
    let mut chain = Vec::new();
    let mut seen = FxHashMap::default();
    let mut base;
    loop {
        if let Some(index) = seen.get(&name).copied() {
            // Every cycle member extends decimal independently (§3.1.7).
            chain.truncate(index + 1);
            base = builtins()["decimal"].clone();
            break;
        }
        if chain.len() >= 128 {
            base = builtins()["decimal"].clone();
            break;
        }
        let style = styles
            .get(&name)
            .map(|(_, s)| s)
            .filter(|s| s.valid())
            .or_else(|| builtins().get(&name));
        let Some(style) = style else {
            if chain.is_empty() {
                return None;
            }
            base = builtins()["decimal"].clone();
            break;
        };
        let Some(parent) = style
            .value("system")
            .and_then(|s| s.strip_prefix("extends "))
        else {
            base = style.clone();
            break;
        };
        seen.insert(name, chain.len());
        chain.push(style);
        name = normalize_name(parent);
    }
    for style in chain.into_iter().rev() {
        base.descriptors.extend(
            style
                .descriptors
                .iter()
                .filter(|(k, _)| *k != "system")
                .map(|(k, v)| (k.clone(), v.clone())),
        );
    }
    Some(base)
}
pub(super) fn representation(name: &str, value: i64, styles: &Styles, marker: bool) -> String {
    if name.eq_ignore_ascii_case("none") {
        return String::new();
    }
    if let Some(text) = unquote_css(name) {
        return text;
    }
    let mut name = normalize_name(name);
    let mut seen = FxHashSet::default();
    let mut original = resolve_style(&name, styles, &mut vec![]);
    if let Some(inner) = name
        .strip_prefix("symbols(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let tokens = split_top_level_ws(inner);
        let (system, first) = if tokens.first().is_some_and(|t| {
            matches!(
                *t,
                "cyclic" | "numeric" | "alphabetic" | "symbolic" | "fixed"
            )
        }) {
            (tokens[0], 1)
        } else {
            ("symbolic", 0)
        };
        original = Some(CounterStyle::parse(&format!(
            "system:{system};symbols:{};suffix: \" \"",
            tokens[first..].join(" ")
        )));
    }
    let mut current = original.clone();
    let result = loop {
        if !seen.insert(name.clone()) {
            break value.to_string();
        }
        let Some(style) = current else {
            break value.to_string();
        };
        if let Some(text) = style.initial(value) {
            break text;
        }
        name = normalize_name(style.value("fallback").unwrap_or("decimal"));
        current = resolve_style(&name, styles, &mut vec![]);
    };
    if marker {
        let prefix = original
            .as_ref()
            .and_then(|s| s.value("prefix"))
            .and_then(symbol)
            .unwrap_or_default();
        let suffix = original
            .as_ref()
            .and_then(|s| s.value("suffix"))
            .and_then(symbol)
            .unwrap_or_else(|| ". ".into());
        format!("{prefix}{result}{suffix}")
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counter_styles_use_normative_symbols_ranges_and_fallbacks() {
        let styles = Styles::default();
        for (name, n, expected) in [
            ("decimal", -7, "-7"),
            ("decimal-leading-zero", -7, "-7"),
            ("decimal-leading-zero", 7, "07"),
            ("lower-alpha", 27, "aa"),
            ("upper-roman", 3999, "MMMCMXCIX"),
            ("upper-roman", 4000, "4000"),
            ("arabic-indic", 12, "١٢"),
            ("hiragana", 1, "あ"),
            ("lower-greek", 1, "α"),
            ("japanese-informal", 42, "四十二"),
            ("simp-chinese-informal", 1010, "一千零一十"),
            ("trad-chinese-formal", -12, "負壹拾貳"),
            ("ethiopic-numeric", 780100000092, "፸፰፻፩፼፼፺፪"),
        ] {
            assert_eq!(representation(name, n, &styles, false), expected, "{name}");
        }
        for (name, style) in builtins() {
            assert!(style.valid(), "normative {name}: {style:?}");
        }
        let mut custom = Styles::default();
        custom.insert("binary".into(),(0,CounterStyle::parse("system:numeric;symbols:'0' '1';negative:'(' ')';pad:8 '0';prefix:'[';suffix:']'")));
        assert_eq!(representation("binary", -3, &custom, false), "(000011)");
        assert_eq!(representation("binary", -3, &custom, true), "[(000011)]");
        custom.insert(
            "fixed".into(),
            (
                0,
                CounterStyle::parse("system:fixed -1;symbols:A B C;fallback:binary"),
            ),
        );
        assert_eq!(representation("fixed", -1, &custom, false), "A");
        assert_eq!(representation("fixed", 2, &custom, false), "00000010");
        custom.insert(
            "unrepresentable".into(),
            (
                0,
                CounterStyle::parse(
                    "system:additive;additive-symbols:3 'a',2 'b';fallback:decimal",
                ),
            ),
        );
        assert_eq!(representation("unrepresentable", 4, &custom, false), "4");
        custom.insert(
            "a".into(),
            (0, CounterStyle::parse("system:extends b;prefix:'a'")),
        );
        custom.insert(
            "b".into(),
            (0, CounterStyle::parse("system:extends a;suffix:'b'")),
        );
        assert_eq!(representation("a", 1, &custom, true), "a1. ");
        assert_eq!(representation("b", 1, &custom, true), "1b");
        assert_eq!(
            representation("symbols(cyclic 'A' 'B')", 0, &styles, false),
            "B"
        );
    }
}
