use super::{
    decode::{self, Lattice, Span},
    dictionary::Entry,
    features::by_name,
    *,
};

fn code(code: &str) -> CountryCode {
    CountryCode::parse(code).expect("country code")
}

fn entry(count: u64, country: &str) -> Entry {
    Entry {
        count,
        country: CountryCode::parse(country),
    }
}

/// A small Pack's worth of names: one Toronto street and its areas, and a
/// French street and town.
fn dictionaries() -> Dictionaries {
    Dictionaries::from_entries([
        (Field::Street, "king street west", entry(400, "CA")),
        (Field::Street, "king street east", entry(300, "CA")),
        (Field::Street, "rue jules raimu", entry(10, "FR")),
        (Field::Locality, "toronto", entry(90_000, "CA")),
        (Field::Locality, "le mans", entry(5_000, "FR")),
        (Field::Region, "ontario", entry(100_000, "CA")),
        (Field::Region, "on", entry(80_000, "CA")),
        (Field::Country, "canada", entry(100_000, "CA")),
        (Field::Country, "ca", entry(100_000, "CA")),
        (Field::StreetType, "street", entry(1, "")),
        (Field::StreetType, "st", entry(1, "")),
        (Field::StreetType, "rue", entry(1, "")),
        (Field::StreetType, "str", entry(1, "")),
        (Field::Unit, "unit", entry(1, "")),
        (Field::Unit, "apt", entry(1, "")),
        (Field::Word, "king", entry(2, "")),
        (Field::Word, "street", entry(2, "")),
        (Field::Word, "west", entry(1, "")),
        (Field::Word, "east", entry(1, "")),
        (Field::Word, "toronto", entry(1, "")),
        (Field::Word, "rue", entry(1, "")),
        (Field::Word, "jules", entry(1, "")),
        (Field::Word, "raimu", entry(1, "")),
        (Field::Word, "le", entry(1, "")),
        (Field::Word, "mans", entry(1, "")),
        (Field::Word, "ontario", entry(1, "")),
        (Field::Word, "on", entry(1, "")),
        (Field::Word, "canada", entry(1, "")),
    ])
}

fn tagger() -> LatticeTagger {
    LatticeTagger::new(dictionaries(), Weights::built_in())
}

fn labels(tagged: &[Tagged]) -> Vec<Label> {
    tagged.iter().map(|tagged| tagged.label).collect()
}

#[test]
fn tokens_are_the_index_words_with_the_field_breaks_between_them() {
    let tokens = Tokens::new("Unit 602, 670 King St. W;Toronto\nON M5V-1M9");
    assert_eq!(
        tokens.words,
        [
            "unit", "602", "670", "king", "st", "w", "toronto", "on", "m5v", "1m9"
        ]
    );
    assert_eq!(
        tokens.breaks,
        [
            false, false, true, false, false, false, true, true, false, false
        ]
    );
    assert_eq!(
        tokens.words.join(" "),
        crate::text_index::normalize_index_text("Unit 602, 670 King St. W;Toronto\nON M5V-1M9")
            .unwrap()
    );
    // A break before the first word is no break.
    assert_eq!(Tokens::new(", toronto").breaks, [false]);
    assert!(Tokens::new(" ,; ").is_empty());
}

#[test]
fn the_lattice_has_every_token_and_the_spans_the_dictionaries_know() {
    let tagger = tagger();
    let tokens = Tokens::new("670 king st w toronto on m5v 1m9");
    let lattice = tagger.lattice(&tokens, &Suppression::default());
    let spans = lattice
        .spans
        .iter()
        .map(|span| (span.start, span.end))
        .collect::<Vec<_>>();
    for index in 0..tokens.len() {
        assert!(spans.contains(&(index, index + 1)), "token {index}");
    }
    // "king st w" is "king street west" once expanded, and the postcode is
    // one span written with or without its space.
    assert!(spans.contains(&(1, 4)), "{spans:?}");
    assert!(spans.contains(&(6, 8)), "{spans:?}");
    assert!(!spans.contains(&(0, 2)), "{spans:?}");
    assert_eq!(lattice.candidates[0], code("CA"));
    assert!(lattice.candidates.len() <= 3);
}

#[test]
fn spans_the_pack_does_not_explain_as_places_or_postcodes_say_so() {
    let tagger = tagger();
    let tokens = Tokens::new("670 nowhere toronto m5v 1m9");
    let lattice = tagger.lattice(&tokens, &Suppression::default());
    let features = |start, end| {
        &lattice
            .spans
            .iter()
            .find(|span| (span.start, span.end) == (start, end))
            .expect("span")
            .features
    };
    let [no_locality, no_region, no_country, no_postcode] =
        ["no_locality", "no_region", "no_country", "no_postcode"]
            .map(|name| by_name(&format!("grounding:{name}")));
    for feature in [no_locality, no_region, no_country, no_postcode] {
        assert!(features(1, 2).contains(&feature));
    }
    assert!(!features(2, 3).contains(&no_locality), "a known locality");
    assert!(features(2, 3).contains(&no_region), "but no region");
    assert!(!features(3, 5).contains(&no_postcode), "a postcode pattern");
    assert!(features(3, 5).contains(&no_locality));
}

#[test]
fn hidden_dictionary_hits_leave_only_the_token_spans() {
    let tagger = tagger();
    let tokens = Tokens::new("670 king street west");
    let mut suppression = Suppression::default();
    suppression.hide(Field::Street, 1, 4);
    let lattice = tagger.lattice(&tokens, &suppression);
    assert!(lattice.spans.iter().all(|span| span.end - span.start == 1));
}

#[test]
fn a_run_of_glued_characters_is_an_edge_whatever_its_length() {
    let tagger = tagger();
    // Two runs, split by the field break; the second is longer than any
    // span looked up.
    let tokens = Tokens::new("み な と み ら い, 神奈川県横浜市西区みなとみらい");
    let units = Units::of(&tokens);
    assert_eq!(units.runs(), [(0, 6), (6, 21)]);
    let lattice = tagger.lattice(&tokens, &Suppression::default());
    let spans = lattice
        .spans
        .iter()
        .map(|span| (span.start, span.end))
        .collect::<Vec<_>>();
    assert!(spans.contains(&(0, 6)), "{spans:?}");
    assert!(spans.contains(&(6, 21)), "{spans:?}");
    let run = lattice
        .spans
        .iter()
        .find(|span| (span.start, span.end) == (6, 21))
        .expect("run");
    assert!(
        run.features
            .iter()
            .any(|id| features::name(*id).starts_with("affix:suffix4:")),
        "the run carries its affixes"
    );
}

#[test]
fn fuzzy_lookup_corrects_long_unknown_words_only() {
    let tagger = tagger();
    let lookup = |field: Field, word: &str| {
        let info = features::TokenInfo::new(word, tagger.dictionaries());
        tagger.lookup(field, &mut SpanKey::new(word.to_string(), &[info]))
    };
    let hit = lookup(Field::Locality, "torontoo").expect("one edit from toronto");
    assert_eq!(hit.distance, 1);
    assert_eq!(hit.entry.country, Some(code("CA")));
    // Three letters are too short to correct.
    assert!(lookup(Field::Street, "kng").is_none());
    // Known words are no typo: "king" is not corrected to anything.
    assert!(lookup(Field::Locality, "king").is_none());
    // A typo in the first letter is not corrected.
    assert!(lookup(Field::Locality, "roronto").is_none());
    // Nor is a name of a script written without spaces.
    let tagger = LatticeTagger::new(
        Dictionaries::from_entries([(Field::Locality, "千代田区", entry(10, "JP"))]),
        Weights::built_in(),
    );
    let info = |word: &str| features::TokenInfo::new(word, tagger.dictionaries());
    let span = ["千", "代", "田", "町"].map(info);
    assert!(
        tagger
            .lookup(Field::Locality, &mut SpanKey::new("千代田町".into(), &span))
            .is_none()
    );
}

#[test]
fn a_toy_model_decodes_the_best_labelling() {
    use Label::*;
    // Two tokens, each a span, and one span over both. Digits are a house
    // number, letters a street, and the two-token span a locality.
    let mut weights = Weights::zero();
    let set = |weights: &mut Weights, feature: &str, label: Label, value: f32| {
        weights.edge[by_name(feature) as usize * decode::LABELS + label as usize] = value;
    };
    set(&mut weights, "shape:digits", HouseNumber, 2.0);
    set(&mut weights, "shape:alpha", Street, 1.0);
    set(&mut weights, "span_tokens:2", Locality, 2.5);
    let span = |start, end, feature: &str| Span {
        start,
        end,
        features: vec![by_name(feature)],
        country_features: Vec::new(),
    };
    let lattice = Lattice::new(
        Units::of(&Tokens::from_words(&["12", "main"])),
        vec![
            span(0, 1, "shape:digits"),
            span(1, 2, "shape:alpha"),
            span(0, 2, "span_tokens:2"),
        ],
        Vec::new(),
    );
    let paths = decode::best(&lattice, &weights, 3);
    assert_eq!(paths[0].labels(&lattice), [HouseNumber, Street]);
    assert_eq!(paths[0].score, 3.0);
    assert_eq!(paths[1].labels(&lattice), [Locality, Locality]);
    assert_eq!(paths[1].score, 2.5);
    // A transition from the house number to the street can outweigh it.
    weights.global[(HouseNumber as usize) * decode::STATES + Street as usize] = -1.0;
    let paths = decode::best(&lattice, &weights, 1);
    assert_eq!(paths[0].labels(&lattice), [Locality, Locality]);
}

#[test]
fn n_best_paths_are_distinct_labellings_best_first() {
    let tagger = tagger();
    let parses = tagger.parse("670 king st w toronto");
    assert_eq!(parses.len(), N_BEST);
    for pair in parses.windows(2) {
        assert!(pair[0].score >= pair[1].score);
        assert_ne!(labels(&pair[0].tokens), labels(&pair[1].tokens));
    }
    // Decoding with the gold labels gives the score of that labelling.
    let tokens = Tokens::new("670 king st w toronto");
    let lattice = tagger.lattice(&tokens, &Suppression::default());
    let gold = labels(&parses[1].tokens);
    let forced = decode::decode(
        &lattice,
        tagger.weights(),
        parses[1].country,
        1,
        Some(&gold),
    );
    assert_eq!(forced[0].labels(&lattice), gold);
    assert!((forced[0].score - parses[1].score).abs() < 1e-4);
}

#[test]
fn a_query_pointing_to_a_country_is_read_as_that_country_only() {
    let tagger = tagger();
    // Every hit is Canadian: no reading leaves the Canadian evidence out.
    let parses = tagger.parse("670 king st w toronto");
    assert!(!parses.is_empty());
    assert!(
        parses
            .iter()
            .all(|parsed| parsed.country == Some(code("CA")))
    );
    // Nothing points to a country: the global fallback reads it.
    let parses = tagger.parse("12 nowhere lane");
    assert!(parses.iter().all(|parsed| parsed.country.is_none()));
}

#[test]
fn labellings_whose_places_cannot_be_one_address_are_dropped() {
    use Label::*;
    let tagger = LatticeTagger::new(
        Dictionaries::from_entries([
            (Field::Region, "愛知県", entry(200_000, "JP")),
            (Field::Region, "京都府", entry(100_000, "JP")),
            (Field::Locality, "名古屋市", entry(90_000, "JP")),
            (Field::Locality, "京都市", entry(60_000, "JP")),
        ])
        .with_areas(
            &[
                ("愛知県", &[1]),
                ("名古屋市", &[2]),
                ("京都府", &[3]),
                ("京都市", &[4]),
            ],
            &[(1, 2), (3, 4)],
        ),
        Weights::built_in(),
    );
    let read = |query: &str| {
        tagger
            .parse(query)
            .into_iter()
            .map(|parsed| {
                parsed
                    .pieces
                    .into_iter()
                    .map(|piece| (piece.token, piece.label))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let city_and_region =
        |city: &str, region: &str| vec![(city.to_string(), Locality), (region.to_string(), Region)];
    // A city in its region is read as one; a city of another region never.
    assert_eq!(
        read("名古屋市, 愛知県")[0],
        city_and_region("名古屋市", "愛知県")
    );
    let parses = read("京都市, 愛知県");
    assert!(!parses.is_empty());
    assert!(
        !parses.contains(&city_and_region("京都市", "愛知県")),
        "{parses:?}"
    );
    // Two places of one level, or two region fields, are no address.
    let lattice = tagger.lattice(
        &Tokens::new("愛知県, 名古屋市, 京都府"),
        &Suppression::default(),
    );
    let one_address = |gold: &[Label]| {
        let gold = lattice.units.expand(gold);
        let path = decode::decode(&lattice, tagger.weights(), Some(code("JP")), 1, Some(&gold));
        tagger.is_one_address(&lattice, &path[0])
    };
    assert!(one_address(&[Region, Locality, Other]));
    assert!(!one_address(&[Region, Locality, Region]));
    assert!(!one_address(&[Region, Locality, Locality]));
    assert!(!one_address(&[Region, Other, Region]));
}

#[test]
fn built_in_weights_read_common_address_shapes() {
    use Label::*;
    let tagger = tagger();
    for (query, expected) in [
        (
            "670 king st w toronto",
            vec![HouseNumber, Street, Street, Street, Locality],
        ),
        (
            "unit 602 670 king st w toronto on m5v 1m9",
            vec![
                Unit,
                Unit,
                HouseNumber,
                Street,
                Street,
                Street,
                Locality,
                Region,
                Postcode,
                Postcode,
            ],
        ),
        (
            "80 rue jules raimu le mans 72100",
            vec![
                HouseNumber,
                Street,
                Street,
                Street,
                Locality,
                Locality,
                Postcode,
            ],
        ),
        ("maximilianstr 5", vec![Street, HouseNumber]),
    ] {
        assert_eq!(labels(&tagger.tag(query)), expected, "{query}");
    }
}

#[test]
fn queries_without_words_tag_nothing() {
    let tagger = tagger();
    assert!(tagger.tag("").is_empty());
    assert!(tagger.parse(" , ").is_empty());
    // A Pack without dictionaries still labels every token.
    let empty = LatticeTagger::new(Dictionaries::empty(), Weights::built_in());
    assert_eq!(empty.tag("670 king street").len(), 3);
}

#[test]
fn weights_round_trip_and_refuse_other_layouts() {
    let mut weights = Weights::built_in();
    let mut table = vec![0.0; decode::STATES * decode::STATES];
    table[3] = 1.5;
    weights.countries.insert(code("CA"), table);
    let bytes = weights.to_bytes();
    assert_eq!(Weights::from_bytes(&bytes).expect("round trip"), weights);
    assert!(weights.knows(code("CA")) && !weights.knows(code("FR")));
    assert_eq!(
        weights.transitions(Some(code("CA"))).get(0, 3),
        weights.global[3] + 1.5
    );

    let mut other_schema = bytes.clone();
    other_schema[12] ^= 1;
    assert!(Weights::from_bytes(&other_schema).is_err());
    assert!(Weights::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    assert!(Weights::from_bytes(b"not weights").is_err());
}

#[test]
fn a_number_written_in_parts_is_one_house_number() {
    use Label::*;
    let tagger = tagger();
    let tokens = Tokens::new("1384 1/2 Queen St, 306-1333 King");
    assert_eq!(
        tokens.joiners,
        [None, None, Some('/'), None, None, None, Some('-'), None]
    );
    for (query, expected) in [
        (
            "1384 1/2 queen st e",
            vec![
                HouseNumber,
                HouseNumber,
                HouseNumber,
                Street,
                Street,
                Street,
            ],
        ),
        (
            "993.5 king st w",
            vec![HouseNumber, HouseNumber, Street, Street, Street],
        ),
        (
            "306-1333 king st w",
            vec![Unit, HouseNumber, Street, Street, Street],
        ),
    ] {
        let parsed = tagger.parse(query);
        assert_eq!(labels(&parsed[0].tokens), expected, "{query}: {parsed:?}");
    }
}

#[test]
fn unspaced_scripts_split_into_glued_characters() {
    let words = |units: &Units| units.tokens.words.clone();
    let unspaced = Units::of(&Tokens::new("東京都千代田区丸の内2-7-2"));
    assert_eq!(
        words(&unspaced),
        [
            "東", "京", "都", "千", "代", "田", "区", "丸", "の", "内", "2", "7", "2"
        ]
    );
    assert_eq!(unspaced.token_of, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2]);
    // The digit run is glued to the character before it in one token; the
    // numbers after the hyphens are not.
    assert_eq!(
        unspaced.glued,
        [
            false, true, true, true, true, true, true, true, true, true, true, false, false
        ]
    );
    assert_eq!(unspaced.key(0, 7), "東京都千代田区");
    // A space between two characters of such a script, or between one and
    // a digit, is no word break, so the spaced and the
    // one-character-per-token forms are the same units.
    for text in [
        "東京都 千代田区 丸の内 2-7-2",
        "東 京 都 千 代 田 区 丸 の 内 2-7-2",
    ] {
        let units = Units::of(&Tokens::new(text));
        assert_eq!(words(&units), words(&unspaced), "{text}");
        assert_eq!(units.glued, unspaced.glued, "{text}");
        assert_eq!(units.key(0, 7), "東京都千代田区", "{text}");
        assert_eq!(units.key(7, 11), "丸の内2", "{text}");
    }
    // A field break still separates them, and spaced scripts stay words.
    let broken = Units::of(&Tokens::new("東京都, 千代田区"));
    assert!(!broken.glued[3]);
    let latin = Units::of(&Tokens::new("670 King St W"));
    assert_eq!(words(&latin), ["670", "king", "st", "w"]);
    assert_eq!(latin.glued, [false; 4]);
    assert_eq!(Units::of(&Tokens::new("서울 종로구")).len(), 2);
    // A digit joins only a script without spaces, and two numbers stay two.
    let mixed = Units::of(&Tokens::new("tower 3 丁目 1 71"));
    assert_eq!(words(&mixed), ["tower", "3", "丁", "目", "1", "71"]);
    assert_eq!(mixed.glued, [false, false, true, true, true, false]);
}

/// The units of `text` as they key and feature a lattice: their words, which
/// are glued, and the field breaks and joiners before them.
fn unit_shape(text: &str) -> (Vec<String>, Vec<bool>, Vec<bool>, Vec<Option<char>>) {
    let units = Units::of(&Tokens::new(text));
    (
        units.tokens.words,
        units.glued,
        units.tokens.breaks,
        units.tokens.joiners,
    )
}

/// The label of every unit of `text` in each of its best labellings.
fn unit_labels(tagger: &LatticeTagger, text: &str) -> Vec<Vec<Label>> {
    let lattice = tagger.lattice(&Tokens::new(text), &Suppression::default());
    decode::best(&lattice, tagger.weights(), N_BEST)
        .iter()
        .map(|path| path.labels(&lattice))
        .collect()
}

#[test]
fn spaced_and_unspaced_writing_are_the_same_units_and_labels() {
    let tagger = LatticeTagger::new(
        Dictionaries::from_entries([
            (Field::Region, "愛知県", entry(200_000, "JP")),
            (Field::Locality, "名古屋市", entry(90_000, "JP")),
            (Field::Locality, "中区", entry(9_000, "JP")),
            (Field::Locality, "4丁目", entry(30_000, "JP")),
            (Field::Poi, "アニメイト", entry(40, "JP")),
            (Field::Street, "ถนนสุขุมวิท", entry(500, "TH")),
            (Field::Locality, "ວຽງຈັນ", entry(500, "LA")),
            (Field::Locality, "ភ្នំពេញ", entry(500, "KH")),
            (Field::Locality, "ရန်ကုန်", entry(500, "MM")),
        ]),
        Weights::built_in(),
    );
    // Han, hiragana, katakana (fullwidth and halfwidth), Thai, Lao, Khmer
    // and Myanmar, each with digits before and after its characters.
    for forms in [
        &[
            "愛知県名古屋市中区栄4丁目5番2号",
            "愛知県 名古屋市 中区 栄 ４丁目 5番2号",
            "愛 知 県 名 古 屋 市 中 区 栄 ４ 丁 目 5 番 2 号",
            "愛知県 名古屋市 中区 栄 4 丁目 5 番 2 号",
        ][..],
        &["４丁目", "４ 丁目", "４ 丁 目", "4丁目"],
        &["5番2号", "5 番 2 号"],
        &["とうきょうとちよだく1", "と う きょう と ち よ だ く 1"],
        &["アニメイト福岡天神3階", "ア ニ メ イ ト 福 岡 天 神 3 階"],
        &["ｱﾆﾒｲﾄ2", "ｱ ﾆ ﾒ ｲ ﾄ 2"],
        &["ถนนสุขุมวิท 15", "ถ น น ส ุ ข ุ ม ว ิ ท 15"],
        &["ນະຄອນຫຼວງວຽງຈັນ 3", "ນ ະ ຄ ອ ນ ຫ ຼ ວ ງ ວ ຽ ງ ຈ ັ ນ 3"],
        &["ភ្នំពេញ 12", "ភ ្ ន ំ ព េ ញ 12"],
        &["ရန်ကုန် 7", "ရ န ် က ု န ် 7"],
    ] {
        let expected = unit_shape(forms[0]);
        let pieces = |text: &str| {
            tagger
                .parse(text)
                .into_iter()
                .map(|parsed| parsed.pieces)
                .collect::<Vec<_>>()
        };
        for form in &forms[1..] {
            assert_eq!(unit_shape(form), expected, "{form} as {}", forms[0]);
            assert_eq!(pieces(form), pieces(forms[0]), "{form}");
            assert_eq!(
                unit_labels(&tagger, form),
                unit_labels(&tagger, forms[0]),
                "{form}"
            );
        }
    }
    assert_eq!(dictionary_key("5 番 2 号"), dictionary_key("５番２号"));
}

#[test]
fn unspaced_scripts_are_the_script_property_of_every_character_listed_before() {
    // The ranges the parser listed by hand before it read the Unicode
    // Script property: Thai, Myanmar, Khmer, the iteration marks, kana, Han
    // and halfwidth katakana. Every letter of them is still unspaced.
    let ranges = [
        0x0E00..=0x0E7F,
        0x1000..=0x109F,
        0x1780..=0x17FF,
        0x3005..=0x3007,
        0x3041..=0x30FF,
        0x31F0..=0x31FF,
        0x3400..=0x4DBF,
        0x4E00..=0x9FFF,
        0xF900..=0xFAFF,
        0xFF66..=0xFF9F,
        0x20000..=0x2A6DF,
    ];
    for code in ranges.into_iter().flatten() {
        let Some(character) = char::from_u32(code) else {
            continue;
        };
        if character.is_alphanumeric() {
            assert!(is_unspaced(character), "U+{code:04X} {character}");
        }
    }
    assert!(is_unspaced('ຫ') && is_unspaced('ー'));
    for character in ['4', '４', 'a', 'ä', 'я', '한', 'ا'] {
        assert!(!is_unspaced(character), "{character}");
    }
}

#[test]
fn units_map_labels_back_to_tokens_and_written_words() {
    use Label::*;
    let units = Units::of(&Tokens::new("東京都千代田区 2"));
    let labels = [
        Region,
        Region,
        Region,
        Locality,
        Locality,
        Locality,
        Locality,
        HouseNumber,
    ];
    // The token's label is the label of most of its characters.
    assert_eq!(units.collapse(&labels), [Locality, HouseNumber]);
    assert_eq!(
        units.expand(&[Street, HouseNumber]),
        [Street; 7]
            .iter()
            .copied()
            .chain([HouseNumber])
            .collect::<Vec<_>>()
    );
    // A name read a character at a time is one word, a name read whole is
    // its own word.
    let mut segments = (0..3)
        .map(|unit| (unit, unit + 1, Region))
        .collect::<Vec<_>>();
    segments.extend([(3, 7, Locality), (7, 8, HouseNumber)]);
    let pieces = units.pieces(&segments);
    assert_eq!(
        pieces
            .iter()
            .map(|piece| (piece.token.as_str(), piece.label))
            .collect::<Vec<_>>(),
        [
            ("東京都", Region),
            ("千代田区", Locality),
            ("2", HouseNumber)
        ]
    );
    let names = Units::of(&Tokens::new("千代田区丸の内"));
    let segments = [
        (0, 4, Locality),
        (4, 5, Locality),
        (5, 6, Locality),
        (6, 7, Locality),
    ];
    let words = names
        .pieces(&segments)
        .into_iter()
        .map(|piece| piece.token)
        .collect::<Vec<_>>();
    assert_eq!(words, ["千代田区", "丸の内"]);
    // Units with a space between them stay words of their own.
    let spaced = Units::of(&Tokens::new("100-0005 king st"));
    let segments = [(0, 2, Postcode), (2, 4, Street)];
    let words = spaced
        .pieces(&segments)
        .into_iter()
        .map(|piece| piece.token)
        .collect::<Vec<_>>();
    assert_eq!(words, ["100", "0005", "king", "st"]);
    // On a tie the first label wins.
    let tie = Units::of(&Tokens::new("丸の内二丁"));
    assert_eq!(
        tie.collapse(&[Street, Street, Locality, Locality, Other]),
        [Street]
    );
}

#[test]
fn names_of_unspaced_scripts_are_found_however_they_are_spaced() {
    use Label::*;
    let tagger = LatticeTagger::new(
        Dictionaries::from_entries([
            (Field::Region, "東京都", entry(200_000, "JP")),
            (Field::Locality, "千代田区", entry(5_000, "JP")),
            (Field::Locality, "丸の内", entry(300, "JP")),
        ]),
        Weights::built_in(),
    );
    for query in [
        "東京都千代田区丸の内",
        "東京都 千代田区 丸の内",
        "東 京 都 千 代 田 区 丸 の 内",
    ] {
        let parsed = &tagger.parse(query)[0];
        assert_eq!(
            parsed
                .pieces
                .iter()
                .map(|piece| (piece.token.as_str(), piece.label))
                .collect::<Vec<_>>(),
            [
                ("東京都", Region),
                ("千代田区", Locality),
                ("丸の内", Locality)
            ],
            "{query}: {parsed:?}"
        );
    }
}

#[test]
fn every_token_of_the_normalized_query_gets_one_label_in_order() {
    let tagger = tagger();
    for query in [
        "東京都千代田区丸の内2-7-2",
        "千代田区 丸の内 2-7-2",
        "100-0005 東京都千代田区丸の内二丁目7番2号",
        "丸の内 2-7-2 千代田区 東京都",
        "tokyo chiyoda marunouchi 2-7-2",
        "東 京 都 千 代 田 区 丸 の 内 2 7 2",
        "Unit 602, 670 King St. W;Toronto\nON M5V-1M9",
        "กรุงเทพมหานคร 10200",
    ] {
        let normalized = crate::text_index::normalize_index_text(query).unwrap();
        let tagged = tagger.tag(query);
        assert_eq!(
            tagged
                .iter()
                .map(|tagged| tagged.token.as_str())
                .collect::<Vec<_>>(),
            normalized.split_whitespace().collect::<Vec<_>>(),
            "{query}"
        );
    }
}

#[test]
fn vocabulary_phrases_are_spans_and_tell_their_neighbours() {
    let dictionaries = Dictionaries::from_entries(
        dictionary::vocabulary("post office box|po box|p o box\n私 書 箱|shishobako")
            .iter()
            .map(|(key, entry)| (Field::PoBox, key.as_str(), *entry)),
    );
    let tagger = LatticeTagger::new(dictionaries, Weights::zero());
    let has = |lattice: &Lattice, start: usize, end: usize, feature: &str| {
        lattice
            .spans
            .iter()
            .find(|span| span.start == start && span.end == end)
            .is_some_and(|span| span.features.contains(&by_name(feature)))
    };
    let lattice = tagger.lattice(&Tokens::new("p o box 29"), &Suppression::default());
    assert!(has(&lattice, 0, 3, "phrase:po_box"));
    assert!(has(&lattice, 3, 4, "phrase_before:po_box"));
    assert!(!has(&lattice, 2, 3, "phrase:po_box"));
    // A phrase of a script written without spaces is its glued units.
    let lattice = tagger.lattice(&Tokens::new("私書箱 819"), &Suppression::default());
    assert!(has(&lattice, 0, 3, "phrase:po_box"));
    assert!(has(&lattice, 3, 4, "phrase_before:po_box"));
}

#[test]
fn glued_area_names_are_a_chain() {
    let dictionaries = Dictionaries::from_entries([
        (Field::Region, "神奈川県", entry(100, "JP")),
        (Field::Locality, "横浜市", entry(50, "JP")),
        (Field::Locality, "西区", entry(10, "JP")),
    ]);
    let tagger = LatticeTagger::new(dictionaries, Weights::zero());
    let lattice = tagger.lattice(&Tokens::new("神奈川県横浜市西区"), &Suppression::default());
    let features = |start: usize, end: usize| {
        lattice
            .spans
            .iter()
            .find(|span| span.start == start && span.end == end)
            .map(|span| span.features.clone())
            .unwrap_or_default()
    };
    assert!(features(0, 9).contains(&by_name("chain:3")));
    assert!(features(0, 9).contains(&by_name("chain:with_region")));
    // Two locality names glued: an edge only because it is a chain.
    assert!(features(4, 9).contains(&by_name("chain:2")));
    assert!(!features(4, 9).contains(&by_name("chain:with_region")));
    // Written apart, the names are no chain.
    let lattice = tagger.lattice(&Tokens::new("横浜市, 西区"), &Suppression::default());
    assert!(
        lattice
            .spans
            .iter()
            .all(|span| !span.features.contains(&by_name("chain:2")))
    );
}

/// The median time to tag a 10-token Latin query on a real Pack:
/// `OG_PARSE_PACK=data/packs/jp cargo test --release tag_latency -- --ignored --nocapture`.
#[test]
#[ignore = "needs a Pack in OG_PARSE_PACK"]
fn tag_latency_on_a_pack() {
    let pack = std::env::var("OG_PARSE_PACK").expect("OG_PARSE_PACK");
    let tagger = LatticeTagger::open(Path::new(&pack)).expect("pack");
    println!("{:?}", tagger.dictionaries());
    let query = "Unit 602, 670 King Street West, Toronto, ON M5V 1M9";
    assert_eq!(Tokens::new(query).len(), 10);
    for _ in 0..200 {
        tagger.tag(query);
    }
    let mut times = (0..2_000)
        .map(|_| {
            let started = std::time::Instant::now();
            std::hint::black_box(tagger.tag(query));
            started.elapsed().as_secs_f64() * 1e6
        })
        .collect::<Vec<_>>();
    times.sort_by(f64::total_cmp);
    println!(
        "median {:.0} us, p90 {:.0} us",
        times[times.len() / 2],
        times[times.len() * 9 / 10]
    );
}

#[test]
fn numbered_names_share_one_form_of_any_numerals() {
    assert_eq!(mask_numerals("5丁目").as_deref(), Some("#丁目"));
    assert_eq!(mask_numerals("四丁目").as_deref(), Some("#丁目"));
    assert_eq!(mask_numerals("１２丁目").as_deref(), Some("#丁目"));
    assert_eq!(mask_numerals("北9条西3").as_deref(), Some("北#条西#"));
    assert_eq!(mask_numerals("sector 12").as_deref(), Some("sector #"));
    assert_eq!(mask_numerals("toronto"), None);
    assert_eq!(mask_numerals("12"), None);
}
