//! `email.utils.getaddresses` (strict mode, as in Debian's Python 3.11) and the old
//! `_parseaddr.AddrlistClass` parser underneath it. reses.py uses it to pull addresses out of
//! To, Cc and the envelope headers when it works out Bcc.

const SPECIALS: &str = "()<>@,:;.\"[]";
const LWS: &str = " \t";
const CR: &str = "\r\n";

struct AddrList {
    field: Vec<char>,
    pos: usize,
    commentlist: Vec<String>,
}

fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn is_atomend(c: char) -> bool {
    SPECIALS.contains(c) || LWS.contains(c) || CR.contains(c)
}

fn is_phraseend(c: char) -> bool {
    c != '.' && is_atomend(c)
}

impl AddrList {
    fn at(&self) -> Option<char> {
        self.field.get(self.pos).copied()
    }

    fn more(&self) -> bool {
        self.pos < self.field.len()
    }

    fn gotonext(&mut self) -> String {
        let mut ws = String::new();
        while let Some(c) = self.at() {
            if " \t\n\r".contains(c) {
                if !"\n\r".contains(c) {
                    ws.push(c);
                }
                self.pos += 1;
            } else if c == '(' {
                let c = self.getcomment();
                self.commentlist.push(c);
            } else {
                break;
            }
        }
        ws
    }

    fn getaddrlist(&mut self) -> Vec<(String, String)> {
        let mut result = Vec::new();
        while self.more() {
            let ad = self.getaddress();
            if ad.is_empty() {
                result.push((String::new(), String::new()));
            } else {
                result.extend(ad);
            }
        }
        result
    }

    fn getaddress(&mut self) -> Vec<(String, String)> {
        self.commentlist.clear();
        self.gotonext();
        let oldpos = self.pos;
        let oldcl = self.commentlist.clone();
        let plist = self.getphraselist();
        self.gotonext();
        let mut returnlist = Vec::new();
        match self.at() {
            None => {
                if let Some(p) = plist.first() {
                    returnlist = vec![(self.commentlist.join(" "), p.clone())];
                }
            }
            Some('.' | '@') => {
                self.pos = oldpos;
                self.commentlist = oldcl;
                let addrspec = self.getaddrspec();
                returnlist = vec![(self.commentlist.join(" "), addrspec)];
            }
            Some(':') => {
                let fieldlen = self.field.len();
                self.pos += 1;
                while self.more() {
                    self.gotonext();
                    if self.pos < fieldlen && self.at() == Some(';') {
                        self.pos += 1;
                        break;
                    }
                    returnlist.extend(self.getaddress());
                }
            }
            Some('<') => {
                let routeaddr = self.getrouteaddr();
                if self.commentlist.is_empty() {
                    returnlist = vec![(plist.join(" "), routeaddr)];
                } else {
                    returnlist = vec![(
                        format!("{} ({})", plist.join(" "), self.commentlist.join(" ")),
                        routeaddr,
                    )];
                }
            }
            Some(c) => {
                if let Some(p) = plist.first() {
                    returnlist = vec![(self.commentlist.join(" "), p.clone())];
                } else if SPECIALS.contains(c) {
                    self.pos += 1;
                }
            }
        }
        self.gotonext();
        if self.at() == Some(',') {
            self.pos += 1;
        }
        returnlist
    }

    fn getrouteaddr(&mut self) -> String {
        if self.at() != Some('<') {
            return String::new();
        }
        let mut expectroute = false;
        self.pos += 1;
        self.gotonext();
        let mut adlist = String::new();
        while let Some(c) = self.at() {
            if expectroute {
                self.getdomain();
                expectroute = false;
            } else if c == '>' {
                self.pos += 1;
                break;
            } else if c == '@' {
                self.pos += 1;
                expectroute = true;
            } else if c == ':' {
                self.pos += 1;
            } else {
                adlist = self.getaddrspec();
                self.pos += 1;
                break;
            }
            self.gotonext();
        }
        adlist
    }

    fn getaddrspec(&mut self) -> String {
        let mut aslist: Vec<String> = Vec::new();
        self.gotonext();
        while let Some(c) = self.at() {
            let mut preserve_ws = true;
            if c == '.' {
                if aslist.last().is_some_and(|l| l.trim().is_empty()) {
                    aslist.pop();
                }
                aslist.push(".".into());
                self.pos += 1;
                preserve_ws = false;
            } else if c == '"' {
                let q = self.getquote();
                aslist.push(format!("\"{}\"", quote(&q)));
            } else if is_atomend(c) {
                if aslist.last().is_some_and(|l| l.trim().is_empty()) {
                    aslist.pop();
                }
                break;
            } else {
                let a = self.getatom(is_atomend);
                aslist.push(a);
            }
            let ws = self.gotonext();
            if preserve_ws && !ws.is_empty() {
                aslist.push(ws);
            }
        }
        if self.at() != Some('@') {
            return aslist.concat();
        }
        aslist.push("@".into());
        self.pos += 1;
        self.gotonext();
        let domain = self.getdomain();
        if domain.is_empty() {
            return String::new();
        }
        aslist.concat() + &domain
    }

    fn getdomain(&mut self) -> String {
        let mut sdlist: Vec<String> = Vec::new();
        while let Some(c) = self.at() {
            if LWS.contains(c) {
                self.pos += 1;
            } else if c == '(' {
                let c = self.getcomment();
                self.commentlist.push(c);
            } else if c == '[' {
                let d = self.getdelimited('[', "]\r", false);
                sdlist.push(format!("[{d}]"));
            } else if c == '.' {
                self.pos += 1;
                sdlist.push(".".into());
            } else if c == '@' {
                return String::new();
            } else if is_atomend(c) {
                break;
            } else {
                let a = self.getatom(is_atomend);
                sdlist.push(a);
            }
        }
        sdlist.concat()
    }

    fn getdelimited(&mut self, beginchar: char, endchars: &str, allowcomments: bool) -> String {
        if self.at() != Some(beginchar) {
            return String::new();
        }
        let mut s = String::new();
        let mut quote = false;
        self.pos += 1;
        while let Some(c) = self.at() {
            if quote {
                s.push(c);
                quote = false;
            } else if endchars.contains(c) {
                self.pos += 1;
                break;
            } else if allowcomments && c == '(' {
                let inner = self.getcomment();
                s.push_str(&inner);
                continue;
            } else if c == '\\' {
                quote = true;
            } else {
                s.push(c);
            }
            self.pos += 1;
        }
        s
    }

    fn getquote(&mut self) -> String {
        self.getdelimited('"', "\"\r", false)
    }

    fn getcomment(&mut self) -> String {
        self.getdelimited('(', ")\r", true)
    }

    fn getatom(&mut self, ends: fn(char) -> bool) -> String {
        let mut a = String::new();
        while let Some(c) = self.at() {
            if ends(c) {
                break;
            }
            a.push(c);
            self.pos += 1;
        }
        a
    }

    fn getphraselist(&mut self) -> Vec<String> {
        let mut plist = Vec::new();
        while let Some(c) = self.at() {
            if " \t\r\n".contains(c) {
                self.pos += 1;
            } else if c == '"' {
                plist.push(self.getquote());
            } else if c == '(' {
                let c = self.getcomment();
                self.commentlist.push(c);
            } else if is_phraseend(c) {
                break;
            } else {
                plist.push(self.getatom(is_phraseend));
            }
        }
        plist
    }
}

fn iter_escaped(addr: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = addr.chars().collect();
    let mut out = Vec::new();
    let mut escape = false;
    let mut pos = 0;
    for (i, &c) in chars.iter().enumerate() {
        pos = i;
        if escape {
            out.push((i, format!("\\{c}")));
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else {
            out.push((i, c.to_string()));
        }
    }
    if escape {
        out.push((pos, "\\".into()));
    }
    out
}

/// `_strip_quoted_realnames`, working on character positions like Python does.
fn strip_quoted_realnames(addr: &str) -> String {
    if !addr.contains('"') {
        return addr.to_string();
    }
    let chars: Vec<char> = addr.chars().collect();
    let mut start = 0;
    let mut open: Option<usize> = None;
    let mut result = String::new();
    for (pos, ch) in iter_escaped(addr) {
        if ch == "\"" {
            match open {
                None => open = Some(pos),
                Some(o) => {
                    if start != o {
                        result.extend(&chars[start..o]);
                    }
                    start = pos + 1;
                    open = None;
                }
            }
        }
    }
    if start < chars.len() {
        result.extend(&chars[start..]);
    }
    result
}

fn check_parenthesis(addr: &str) -> bool {
    let addr = strip_quoted_realnames(addr);
    let mut opens = 0i64;
    for (_, ch) in iter_escaped(&addr) {
        if ch == "(" {
            opens += 1;
        } else if ch == ")" {
            opens -= 1;
            if opens < 0 {
                return false;
            }
        }
    }
    opens == 0
}

/// `email.utils.getaddresses(fieldvalues)` with strict=True.
pub(super) fn getaddresses(fieldvalues: &[String]) -> Vec<(String, String)> {
    let values: Vec<String> = fieldvalues
        .iter()
        .map(|v| {
            if check_parenthesis(v) {
                v.clone()
            } else {
                "('', '')".to_string()
            }
        })
        .collect();
    let joined = values.join(", ");
    let mut parser = AddrList {
        field: joined.chars().collect(),
        pos: 0,
        commentlist: Vec::new(),
    };
    let parsed = if joined.is_empty() {
        Vec::new()
    } else {
        parser.getaddrlist()
    };
    let result: Vec<(String, String)> = parsed
        .into_iter()
        .map(|v| {
            if v.1.contains('[') {
                (String::new(), String::new())
            } else {
                v
            }
        })
        .collect();
    let expected: usize = values
        .iter()
        .map(|v| 1 + strip_quoted_realnames(v).matches(',').count())
        .sum();
    if result.len() != expected {
        return vec![(String::new(), String::new())];
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(v: &[&str]) -> Vec<String> {
        let v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        getaddresses(&v).into_iter().map(|(_, a)| a).collect()
    }

    // Expected values come from running getaddresses in the CI image's Python.
    #[test]
    fn like_python() {
        assert_eq!(
            addrs(&["A <a@example.com>, b@example.org"]),
            ["a@example.com", "b@example.org"]
        );
        assert_eq!(addrs(&["\"Doe, J\" <j@example.com>"]), ["j@example.com"]);
        assert_eq!(
            addrs(&["a@example.com", "b@example.com"]),
            ["a@example.com", "b@example.com"]
        );
        assert_eq!(
            addrs(&["Team: a@example.org, b@example.org;"]),
            ["a@example.org", "b@example.org"]
        );
        assert_eq!(addrs(&["alice@example.com <bob@example.com>"]), [""]);
        assert_eq!(addrs(&["x (unbalanced <x@example.com>"]), [""]);
    }
}
