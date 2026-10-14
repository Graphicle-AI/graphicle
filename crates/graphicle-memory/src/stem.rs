//! Porter (1980) suffix stemmer, following Martin Porter's reference C
//! implementation including its two documented departures (`-bli` → `-ble`,
//! `-logi` → `-log`) and its rule that words of one or two letters are kept.
//! This is the stemmer of Lucene's and Anserini's English BM25 baselines; Mem0
//! (2026) lemmatizes BM25 terms for the same reason: "plans", "planned" and
//! "planning" should match the same query term. Only all-ASCII-lowercase words
//! are stemmed; numbers, identifiers with digits and non-ASCII words pass
//! through unchanged.
use std::collections::HashMap;

struct Porter {
    b: Vec<u8>,
    k: usize,
    j: isize,
}

impl Porter {
    fn cons(&self, i: usize) -> bool {
        // Consecutive y's alternate. Walk the run without recursion: terms
        // come from arbitrary document text and have no word-length limit.
        let prefix = &self.b[..=i];
        let ys = prefix.iter().rev().take_while(|&&b| b == b'y').count();
        let preceding = prefix[..prefix.len() - ys]
            .last()
            .is_some_and(|&b| !matches!(b, b'a' | b'e' | b'i' | b'o' | b'u'));
        preceding ^ (ys % 2 != 0)
    }
    /// Number of vowel-consonant sequences in `b[0..=j]`.
    fn m(&self) -> usize {
        let mut n = 0;
        let mut previous = true;
        // Carry the previous classification instead of rescanning each y
        // prefix; a single long word must take linear, not quadratic, work.
        for (i, &b) in self.b.iter().take((self.j + 1) as usize).enumerate() {
            let consonant = match b {
                b'a' | b'e' | b'i' | b'o' | b'u' => false,
                b'y' => i == 0 || !previous,
                _ => true,
            };
            n += usize::from(consonant && !previous);
            previous = consonant;
        }
        n
    }
    fn vowel_in_stem(&self) -> bool {
        (0..=self.j).any(|i| !self.cons(i as usize))
    }
    fn double_consonant(&self, j: usize) -> bool {
        j >= 1 && self.b[j] == self.b[j - 1] && self.cons(j)
    }
    /// Consonant-vowel-consonant ending at `i`, the last not w, x or y.
    fn cvc(&self, i: isize) -> bool {
        if i < 2 {
            return false;
        }
        let i = i as usize;
        if !self.cons(i) || self.cons(i - 1) || !self.cons(i - 2) {
            return false;
        }
        !matches!(self.b[i], b'w' | b'x' | b'y')
    }
    fn ends(&mut self, s: &str) -> bool {
        let s = s.as_bytes();
        let length = s.len();
        if length > self.k + 1 || &self.b[self.k + 1 - length..=self.k] != s {
            return false;
        }
        self.j = self.k as isize - length as isize;
        true
    }
    fn set_to(&mut self, s: &str) {
        let start = (self.j + 1) as usize;
        self.b.truncate(start);
        self.b.extend_from_slice(s.as_bytes());
        self.k = (self.j + s.len() as isize) as usize;
    }
    fn replace(&mut self, s: &str) {
        if self.m() > 0 {
            self.set_to(s);
        }
    }
    fn step1ab(&mut self) {
        if self.b[self.k] == b's' {
            if self.ends("sses") {
                self.k -= 2;
            } else if self.ends("ies") {
                self.set_to("i");
            } else if self.k >= 1 && self.b[self.k - 1] != b's' {
                self.k -= 1;
            }
            self.b.truncate(self.k + 1);
        }
        if self.ends("eed") {
            if self.m() > 0 {
                self.k -= 1;
                self.b.truncate(self.k + 1);
            }
        } else if (self.ends("ed") || self.ends("ing")) && self.vowel_in_stem() {
            self.k = self.j as usize;
            self.b.truncate(self.k + 1);
            if self.ends("at") {
                self.set_to("ate");
            } else if self.ends("bl") {
                self.set_to("ble");
            } else if self.ends("iz") {
                self.set_to("ize");
            } else if self.double_consonant(self.k) {
                if !matches!(self.b[self.k], b'l' | b's' | b'z') {
                    self.k -= 1;
                    self.b.truncate(self.k + 1);
                }
            } else {
                self.j = self.k as isize;
                if self.m() == 1 && self.cvc(self.k as isize) {
                    self.set_to("e");
                }
            }
        }
    }
    fn step1c(&mut self) {
        if self.ends("y") && self.vowel_in_stem() {
            self.b[self.k] = b'i';
        }
    }
    fn step2(&mut self) {
        const RULES: &[(&str, &str)] = &[
            ("ational", "ate"),
            ("tional", "tion"),
            ("enci", "ence"),
            ("anci", "ance"),
            ("izer", "ize"),
            ("bli", "ble"),
            ("alli", "al"),
            ("entli", "ent"),
            ("eli", "e"),
            ("ousli", "ous"),
            ("ization", "ize"),
            ("ation", "ate"),
            ("ator", "ate"),
            ("alism", "al"),
            ("iveness", "ive"),
            ("fulness", "ful"),
            ("ousness", "ous"),
            ("aliti", "al"),
            ("iviti", "ive"),
            ("biliti", "ble"),
            ("logi", "log"),
        ];
        self.rules(RULES, 1);
    }
    fn step3(&mut self) {
        const RULES: &[(&str, &str)] = &[
            ("icate", "ic"),
            ("ative", ""),
            ("alize", "al"),
            ("iciti", "ic"),
            ("ical", "ic"),
            ("ful", ""),
            ("ness", ""),
        ];
        self.rules(RULES, 0);
    }
    /// Applies the first rule whose suffix matches (and whose key letter, the
    /// letter `offset` before the end, agrees, as in the reference switch).
    fn rules(&mut self, rules: &[(&str, &str)], offset: usize) {
        if self.k < offset {
            return;
        }
        let key = self.b[self.k - offset];
        for (suffix, replacement) in rules {
            let s = suffix.as_bytes();
            if s.len() <= offset || s[s.len() - 1 - offset] != key {
                continue;
            }
            if self.ends(suffix) {
                self.replace(replacement);
                return;
            }
        }
    }
    fn step4(&mut self) {
        if self.k < 1 {
            return;
        }
        let matched = match self.b[self.k - 1] {
            b'a' => self.ends("al"),
            b'c' => self.ends("ance") || self.ends("ence"),
            b'e' => self.ends("er"),
            b'i' => self.ends("ic"),
            b'l' => self.ends("able") || self.ends("ible"),
            b'n' => self.ends("ant") || self.ends("ement") || self.ends("ment") || self.ends("ent"),
            b'o' => {
                (self.ends("ion") && self.j >= 0 && matches!(self.b[self.j as usize], b's' | b't'))
                    || self.ends("ou")
            }
            b's' => self.ends("ism"),
            b't' => self.ends("ate") || self.ends("iti"),
            b'u' => self.ends("ous"),
            b'v' => self.ends("ive"),
            b'z' => self.ends("ize"),
            _ => false,
        };
        if matched && self.m() > 1 {
            self.k = self.j as usize;
            self.b.truncate(self.k + 1);
        }
    }
    fn step5(&mut self) {
        self.j = self.k as isize;
        if self.b[self.k] == b'e' {
            let a = self.m();
            if a > 1 || (a == 1 && !self.cvc(self.k as isize - 1)) {
                self.k -= 1;
                self.b.truncate(self.k + 1);
            }
        }
        // The reference still measures up to the removed `e`; a trailing
        // vowel never changes the measure, so measuring up to `k` agrees.
        self.j = self.k as isize;
        if self.b[self.k] == b'l' && self.double_consonant(self.k) && self.m() > 1 {
            self.k -= 1;
            self.b.truncate(self.k + 1);
        }
    }
}

/// The Porter stem of `word`, or `word` itself when it is not all ASCII
/// lowercase letters or has at most two letters.
pub fn stem(word: &str) -> String {
    if word.len() <= 2 || !word.bytes().all(|c| c.is_ascii_lowercase()) {
        return word.to_owned();
    }
    let mut p = Porter {
        b: word.as_bytes().to_vec(),
        k: word.len() - 1,
        j: 0,
    };
    p.step1ab();
    if p.k > 0 {
        p.step1c();
        p.step2();
        p.step3();
        p.step4();
        p.step5();
    }
    p.b.truncate(p.k + 1);
    String::from_utf8(p.b).expect("ASCII stays ASCII")
}

/// Memoized [`stem`] for one ranking call: a candidate set repeats the same
/// few thousand words across thousands of rows.
#[derive(Default)]
pub struct Stemmer {
    memo: HashMap<String, String>,
}

impl Stemmer {
    pub fn stem(&mut self, word: &str) -> String {
        if let Some(found) = self.memo.get(word) {
            return found.clone();
        }
        let value = stem(word);
        self.memo.insert(word.to_owned(), value.clone());
        value
    }
}

#[cfg(test)]
mod tests {
    use super::stem;

    #[test]
    fn porter_paper_and_reference_vocabulary() {
        let cases = [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("caress", "caress"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("bled", "bled"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("hopping", "hop"),
            ("tanned", "tan"),
            ("falling", "fall"),
            ("hissing", "hiss"),
            ("fizzed", "fizz"),
            ("failing", "fail"),
            ("filing", "file"),
            ("happy", "happi"),
            ("sky", "sky"),
            ("relational", "relat"),
            ("generalizations", "gener"),
            ("oscillators", "oscil"),
            ("connection", "connect"),
            ("connections", "connect"),
            ("connective", "connect"),
            ("connected", "connect"),
            ("connecting", "connect"),
            ("running", "run"),
            ("runs", "run"),
            ("camping", "camp"),
            ("recommendations", "recommend"),
            ("controll", "control"),
            ("roll", "roll"),
            ("probate", "probat"),
            ("rate", "rate"),
            ("cease", "ceas"),
            ("abandoned", "abandon"),
            ("ability", "abil"),
            ("abominable", "abomin"),
            ("absolutely", "absolut"),
            ("accompanied", "accompani"),
            ("achievement", "achiev"),
            ("activities", "activ"),
            ("sensibility", "sensibl"),
            ("analogical", "analog"),
            ("andouille", "andouil"),
            ("alle", "all"),
        ];
        for (word, expected) in cases {
            assert_eq!(stem(word), expected, "{word}");
        }
    }

    #[test]
    fn short_numeric_and_non_ascii_words_pass_through() {
        for word in ["is", "as", "2023", "b12", "café", "naïve", "x", ""] {
            assert_eq!(stem(word), word);
        }
    }

    #[test]
    fn long_y_runs_preserve_porter_rules_without_recursive_scans() {
        let word = "y".repeat(262_144);
        assert_eq!(stem(&format!("{word}ation")), word);
        assert_eq!(stem(&word), format!("{}i", &word[..word.len() - 1]));
        assert_eq!(
            stem(&format!("{word}ed")),
            format!("{}i", &word[..word.len() - 1])
        );
        // The preceding letter determines the alternating classification.
        for (input, expected) in [("ayye", "ayy"), ("byye", "byy"), ("bbye", "bbye")] {
            assert_eq!(stem(input), expected);
        }
    }

    /// Cross-checks against a `word<TAB>stem` file produced by NLTK's
    /// `PorterStemmer(mode=PorterStemmer.MARTIN_EXTENSIONS)`, which follows
    /// the same reference C implementation:
    /// `GRAPHICLE_STEM_ORACLE=/path/oracle.tsv cargo test --lib stem -- --ignored`
    #[test]
    #[ignore = "needs an NLTK oracle file"]
    fn matches_reference_oracle() {
        let Ok(path) = std::env::var("GRAPHICLE_STEM_ORACLE") else {
            return;
        };
        let text = std::fs::read_to_string(path).expect("oracle file");
        let mut checked = 0;
        let mut wrong = Vec::new();
        for line in text.lines() {
            let Some((word, expected)) = line.split_once('\t') else {
                continue;
            };
            checked += 1;
            let got = stem(word);
            if got != expected {
                wrong.push(format!("{word}: {got} != {expected}"));
            }
        }
        assert!(checked > 0);
        assert!(
            wrong.is_empty(),
            "{} of {checked} differ: {:?}",
            wrong.len(),
            &wrong[..wrong.len().min(40)]
        );
    }
}
