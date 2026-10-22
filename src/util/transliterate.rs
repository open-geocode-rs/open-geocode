//! Latin forms of names written in kana, Cyrillic or Greek.
//!
//! A query names a place in the script its writer types, and a Pack holds
//! the Latin form only when a `name:*-Latn` or `name:en` tag states it. Kana
//! (Hepburn romanization, long vowels written short as signs and maps write
//! them: `とうきょう` is `tokyo`), Cyrillic and Greek are read sound by sound,
//! so the builder adds the Latin form of every such name as one more name.
//! Han characters have no one reading, so a name holding any is left alone,
//! as is a name of any other script without a table here.
//!
//! Hand tables, not CLDR's script-to-Latin transforms: those write scholarly
//! forms nobody types ("selkovo" for "щёлково", "athḗna", "toukyou"), read
//! Japanese kanji as Mandarin, and have no Thai or any-script form. The Latin
//! written here is then folded to ASCII like every other Latin name
//! ([`crate::util::fold::search_key`]).

/// The Latin form of `name`, lower case, or `None` when it holds no kana,
/// Cyrillic or Greek letter, or a letter of a script this does not read.
pub(crate) fn to_latin(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut changed = false;
    let mut kana = Vec::new();
    let mut chars = lower.chars().peekable();
    while let Some(character) = chars.next() {
        if let Some(hiragana) = as_hiragana(character) {
            kana.push(hiragana);
            continue;
        }
        if !kana.is_empty() {
            out.push_str(&romanize_kana(&kana)?);
            kana.clear();
            changed = true;
        }
        if let Some(latin) = greek_digraph(character, chars.peek().copied()) {
            chars.next();
            out.push_str(latin);
            changed = true;
        } else if let Some(latin) = cyrillic(character).or_else(|| greek(character)) {
            out.push_str(latin);
            changed = true;
        } else if character.is_alphabetic() && !is_latin(character) {
            return None;
        } else {
            out.push(character);
        }
    }
    if !kana.is_empty() {
        out.push_str(&romanize_kana(&kana)?);
        changed = true;
    }
    changed.then_some(out)
}

fn is_latin(character: char) -> bool {
    matches!(u32::from(character), 0..=0x24F | 0x1E00..=0x1EFF)
}

/// Kana as hiragana: katakana are the same syllables 0x60 code points up.
/// The long vowel mark and the middle dot are kept as they are.
fn as_hiragana(character: char) -> Option<char> {
    match u32::from(character) {
        0x3041..=0x3096 | 0x309D | 0x309E | 0x30FB | 0x30FC => Some(character),
        code @ (0x30A1..=0x30F6 | 0x30FD | 0x30FE) => char::from_u32(code - 0x60),
        _ => None,
    }
}

/// The Hepburn syllable of one hiragana that is not a small one.
fn syllable(kana: char) -> Option<&'static str> {
    Some(match kana {
        'あ' => "a",
        'い' | 'ゐ' => "i",
        'う' => "u",
        'え' | 'ゑ' => "e",
        'お' | 'を' => "o",
        'か' | 'ゕ' => "ka",
        'き' => "ki",
        'く' => "ku",
        'け' | 'ゖ' => "ke",
        'こ' => "ko",
        'が' => "ga",
        'ぎ' => "gi",
        'ぐ' => "gu",
        'げ' => "ge",
        'ご' => "go",
        'さ' => "sa",
        'し' => "shi",
        'す' => "su",
        'せ' => "se",
        'そ' => "so",
        'ざ' => "za",
        'じ' | 'ぢ' => "ji",
        'ず' | 'づ' => "zu",
        'ぜ' => "ze",
        'ぞ' => "zo",
        'た' => "ta",
        'ち' => "chi",
        'つ' => "tsu",
        'て' => "te",
        'と' => "to",
        'だ' => "da",
        'で' => "de",
        'ど' => "do",
        'な' => "na",
        'に' => "ni",
        'ぬ' => "nu",
        'ね' => "ne",
        'の' => "no",
        'は' => "ha",
        'ひ' => "hi",
        'ふ' => "fu",
        'へ' => "he",
        'ほ' => "ho",
        'ば' => "ba",
        'び' => "bi",
        'ぶ' => "bu",
        'べ' => "be",
        'ぼ' => "bo",
        'ぱ' => "pa",
        'ぴ' => "pi",
        'ぷ' => "pu",
        'ぺ' => "pe",
        'ぽ' => "po",
        'ま' => "ma",
        'み' => "mi",
        'む' => "mu",
        'め' => "me",
        'も' => "mo",
        'や' => "ya",
        'ゆ' => "yu",
        'よ' => "yo",
        'ら' => "ra",
        'り' => "ri",
        'る' => "ru",
        'れ' => "re",
        'ろ' => "ro",
        'わ' => "wa",
        'ん' => "n",
        'ゔ' => "vu",
        _ => return None,
    })
}

/// The vowel of a small kana, and whether it is a glide (small ya, yu, yo).
fn small(kana: char) -> Option<(&'static str, bool)> {
    Some(match kana {
        'ぁ' => ("a", false),
        'ぃ' => ("i", false),
        'ぅ' => ("u", false),
        'ぇ' => ("e", false),
        'ぉ' => ("o", false),
        'ゃ' => ("a", true),
        'ゅ' => ("u", true),
        'ょ' => ("o", true),
        'ゎ' => ("wa", false),
        _ => return None,
    })
}

/// Hepburn romanization of a run of hiragana, long vowels written short.
fn romanize_kana(kana: &[char]) -> Option<String> {
    let mut syllables: Vec<String> = Vec::with_capacity(kana.len());
    let mut double_next = false;
    for &character in kana {
        match character {
            // A small tsu doubles the consonant after it.
            'っ' => {
                double_next = true;
                continue;
            }
            // The long vowel mark: long vowels are written short.
            'ー' => continue,
            '・' => {
                syllables.push(" ".to_string());
                continue;
            }
            // An iteration mark repeats the syllable before it.
            'ゝ' | 'ゞ' => {
                if let Some(last) = syllables.last().cloned() {
                    syllables.push(last);
                }
                continue;
            }
            _ => {}
        }
        if let Some((vowel, glide)) = small(character) {
            let last = syllables.pop().unwrap_or_default();
            syllables.push(join_small(&last, vowel, glide));
            continue;
        }
        let syllable = syllable(character)?;
        let previous = syllables.last().map(String::as_str).unwrap_or_default();
        // A long o or u is written as one vowel: "tou" is "to", "oo" is "o".
        let long = match syllable {
            "u" => previous.ends_with('o') || previous.ends_with('u'),
            "o" => previous.ends_with('o'),
            _ => false,
        };
        if long && !double_next {
            continue;
        }
        let mut written = syllable.to_string();
        if std::mem::take(&mut double_next) {
            match written.as_bytes().first() {
                Some(b'c') => written.insert(0, 't'),
                Some(&first) if !b"aeioun".contains(&first) => written.insert(0, first as char),
                _ => {}
            }
        }
        syllables.push(written);
    }
    Some(syllables.concat())
}

/// A syllable followed by a small kana: "ki" and "ya" is "kya", "shi" and
/// "ya" is "sha", "te" and "i" is "ti", "u" and "i" is "wi".
fn join_small(last: &str, vowel: &str, glide: bool) -> String {
    if last.is_empty() || last == " " {
        let small = if glide {
            format!("y{vowel}")
        } else {
            vowel.to_string()
        };
        return format!("{last}{small}");
    }
    if glide {
        let Some(stem) = last.strip_suffix('i') else {
            return format!("{last}y{vowel}");
        };
        return if stem.ends_with("sh") || stem.ends_with("ch") || stem.ends_with('j') {
            format!("{stem}{vowel}")
        } else {
            format!("{stem}y{vowel}")
        };
    }
    if last == "u" {
        return format!("w{vowel}");
    }
    let stem = last.trim_end_matches(['a', 'e', 'i', 'o', 'u']);
    let stem = if stem.is_empty() { last } else { stem };
    format!("{stem}{vowel}")
}

fn cyrillic(character: char) -> Option<&'static str> {
    Some(match character {
        'а' => "a",
        'б' => "b",
        'в' => "v",
        'г' | 'ґ' => "g",
        'д' => "d",
        'е' | 'ё' | 'э' => "e",
        'ж' => "zh",
        'з' => "z",
        'и' | 'і' => "i",
        'й' | 'ы' => "y",
        'к' => "k",
        'л' => "l",
        'м' => "m",
        'н' => "n",
        'о' => "o",
        'п' => "p",
        'р' => "r",
        'с' => "s",
        'т' => "t",
        'у' | 'ў' => "u",
        'ф' => "f",
        'х' => "kh",
        'ц' => "ts",
        'ч' => "ch",
        'ш' => "sh",
        'щ' => "shch",
        'ъ' | 'ь' => "",
        'ю' => "yu",
        'я' => "ya",
        'ї' => "yi",
        'є' => "ye",
        'ђ' => "dj",
        'ј' => "j",
        'љ' => "lj",
        'њ' => "nj",
        'ћ' => "c",
        'џ' | 'ѕ' => "dz",
        'ѓ' => "gj",
        'ќ' => "kj",
        _ => return None,
    })
}

fn greek(character: char) -> Option<&'static str> {
    Some(match character {
        'α' | 'ά' => "a",
        'β' => "v",
        'γ' => "g",
        'δ' => "d",
        'ε' | 'έ' => "e",
        'ζ' => "z",
        'η' | 'ή' | 'ι' | 'ί' | 'ϊ' | 'ΐ' => "i",
        'θ' => "th",
        'κ' => "k",
        'λ' => "l",
        'μ' => "m",
        'ν' => "n",
        'ξ' => "x",
        'ο' | 'ό' | 'ω' | 'ώ' => "o",
        'π' => "p",
        'ρ' => "r",
        'σ' | 'ς' => "s",
        'τ' => "t",
        'υ' | 'ύ' | 'ϋ' | 'ΰ' => "y",
        'φ' => "f",
        'χ' => "ch",
        'ψ' => "ps",
        _ => return None,
    })
}

/// The Greek vowel pairs read as one sound: "ου" is "ou", "αυ" "av".
fn greek_digraph(character: char, next: Option<char>) -> Option<&'static str> {
    if !matches!(next?, 'υ' | 'ύ') {
        return None;
    }
    Some(match character {
        'ο' | 'ό' => "ou",
        'α' | 'ά' => "av",
        'ε' | 'έ' => "ev",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kana_read_in_hepburn_with_long_vowels_short() {
        for (kana, latin) in [
            ("とうきょう", "tokyo"),
            ("よこはまし", "yokohamashi"),
            ("おおさか", "osaka"),
            ("ほっかいどう", "hokkaido"),
            ("しんじゅく", "shinjuku"),
            ("ちゃや", "chaya"),
            ("きたなごや し", "kitanagoya shi"),
            ("マクドナルド", "makudonarudo"),
            ("ファミリーマート", "famirimato"),
            ("ティー", "ti"),
            ("ウィング", "wingu"),
            ("まっちゃ", "matcha"),
            ("ヴィラ", "vira"),
            ("サッポロ・ビール", "sapporo biru"),
            ("いすゞ", "isusu"),
        ] {
            assert_eq!(to_latin(kana).as_deref(), Some(latin), "{kana}");
        }
    }

    #[test]
    fn cyrillic_and_greek_read_letter_by_letter() {
        for (name, latin) in [
            ("Москва", "moskva"),
            ("Щёлково", "shchelkovo"),
            ("улица Ленина 5", "ulitsa lenina 5"),
            ("Львів", "lviv"),
            ("Αθήνα", "athina"),
            ("Θεσσαλονίκη", "thessaloniki"),
            ("Πλατεία Αυστρίας", "plateia avstrias"),
            ("Ούλα", "oula"),
            ("Hotel Москва", "hotel moskva"),
        ] {
            assert_eq!(to_latin(name).as_deref(), Some(latin), "{name}");
        }
    }

    #[test]
    fn han_latin_and_other_scripts_are_left_alone() {
        for name in ["Toronto", "München", "丸の内", "東京", "서울", "123"] {
            assert_eq!(to_latin(name), None, "{name}");
        }
    }
}
