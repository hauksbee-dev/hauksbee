//! KiCad net-name unescaping: the ONE place file-syntax spellings become the
//! names a person recognises. KiCad's file format escapes reserved characters
//! (`/GPIO0{slash}XTAL1`) and stores sub/superscript render markup verbatim
//! (`SCL_{2}`); both leaked into findings, `--list-nets`, and the JSON
//! contract. Every parser that reads a net name from KiCad text funnels
//! through [`unescape_net_name`], so the internal tables, the copper DRC, and
//! every user-facing surface agree on the same real name.

/// The named escape tokens KiCad's `EscapeString` writes, with their
/// characters. `{slash}` is by far the most common (the `/` sheet-path
/// separator inside a label).
const NAMED_ESCAPES: &[(&str, &str)] = &[
    ("{slash}", "/"),
    ("{colon}", ":"),
    ("{dblquote}", "\""),
    ("{quote}", "'"),
    ("{lt}", "<"),
    ("{gt}", ">"),
    ("{bar}", "|"),
    ("{space}", " "),
    ("{brace}", "{"),
];

/// Turn a net name as spelled in a KiCad file into the name the schematic
/// shows: named escape tokens become their characters, and the sub-/super-
/// script / overbar markup braces (`SCL_{2}`, `V^{2}`, `~{RESET}`) are
/// dropped while their marker character is kept (`SCL_2`, `V^2`, `~RESET`).
/// Names with no `{` pass through untouched (the overwhelmingly common case).
pub fn unescape_net_name(name: &str) -> String {
    if !name.contains('{') {
        return name.to_string();
    }
    let mut s = name.to_string();
    for (tok, ch) in NAMED_ESCAPES {
        if s.contains(tok) {
            s = s.replace(tok, ch);
        }
    }
    // Render-markup braces: `_{...}` / `^{...}` / `~{...}` keep the marker and
    // the content, dropping only the braces. A `{` in any other position is
    // left alone; better an odd brace than a corrupted name.
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let is_marker = matches!(c, '_' | '^' | '~');
        if is_marker && i + 1 < chars.len() && chars[i + 1] == '{' {
            if let Some(close) = chars[i + 2..].iter().position(|&x| x == '}') {
                if c != '~' {
                    out.push(c);
                }
                if c == '~' {
                    out.push('~');
                }
                out.extend(&chars[i + 2..i + 2 + close]);
                i += close + 3;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::unescape_net_name;

    #[test]
    fn slash_escape_becomes_a_real_slash() {
        assert_eq!(
            unescape_net_name("/GPIO0{slash}XTAL1{slash}CLKIN"),
            "/GPIO0/XTAL1/CLKIN"
        );
    }

    #[test]
    fn subscript_markup_drops_the_braces_only() {
        assert_eq!(unescape_net_name("SCL_{2}"), "SCL_2");
        assert_eq!(
            unescape_net_name("Net-(IC506-SDA_{2})"),
            "Net-(IC506-SDA_2)"
        );
        assert_eq!(unescape_net_name("V^{2}"), "V^2");
        assert_eq!(unescape_net_name("~{RESET}"), "~RESET");
    }

    #[test]
    fn plain_names_and_lone_braces_pass_through() {
        assert_eq!(unescape_net_name("GND"), "GND");
        assert_eq!(unescape_net_name("ODD{name"), "ODD{name");
        assert_eq!(unescape_net_name("A_{unclosed"), "A_{unclosed");
    }
}
