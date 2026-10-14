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
    // A space between two characters of such a script is no word break, so
    // the spaced and the one-character-per-token forms are the same units.
    for text in [
        "東京都 千代田区 丸の内 2-7-2",
        "東 京 都 千 代 田 区 丸 の 内 2-7-2",
    ] {
        let units = Units::of(&Tokens::new(text));
        assert_eq!(words(&units), words(&unspaced), "{text}");
        assert_eq!(units.key(0, 7), "東京都千代田区", "{text}");
        assert_eq!(units.key(7, 11), "丸の内 2", "{text}");
    }
    // A field break still separates them, and spaced scripts stay words.
    let broken = Units::of(&Tokens::new("東京都, 千代田区"));
    assert!(!broken.glued[3]);
    let latin = Units::of(&Tokens::new("670 King St W"));
    assert_eq!(words(&latin), ["670", "king", "st", "w"]);
    assert_eq!(latin.glued, [false; 4]);
    assert_eq!(Units::of(&Tokens::new("서울 종로구")).len(), 2);
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
