# Decisions

Why parts of og are built the way they are, and what was tried before.
Each entry names the code it governs, says what og does, lists what was rejected and why, and keeps the evidence that decided it.
Change or delete an entry in the same pull request that changes its code.

## Parsing queries

### A bare pair of whole numbers is not a position

Where. `decimal` in `src/compat/position.rs`.

A decimal query needs a decimal point in both numbers to count as a latitude and longitude.
A bare whole number pair is a house number or a postcode more often than a degree.

Rejected.

- **Accept any two numbers in range as a position.** Whole numbers are far more often a house number or a postcode than a degree.

### A district that is also a city is written as a locality in training rows

Where. `context` in `src/parse/lattice/train.rs`.

A district between a record's locality and region goes on the region side only when its name names no locality.
Gütersloh is a city as well as its Kreis, and a query naming it means the city.
A district with no locality below it is the locality an address names, as with a kreisfreie Stadt.
A city that is a region of its own, such as Berlin or Hamburg, is the locality of the records in it that lie in no other.

Rejected.

- **Always write a district on the region side.** A name like Gütersloh would be labelled as a region when queries mean the city.

Evidence.

Gütersloh, Berlin, Hamburg, kreisfreie Stadt.
The same rule is applied in the PlaceLayer::Region and PlaceLayer::District branches.

### A query is decoded only under its candidate countries, never against a global reading

Where. `countries` and `best` in `src/parse/lattice/decode.rs`.

When dictionary hits and postcode matches point to a country, the lattice is read with that country's evidence and transitions only.
The global fallback is used alone when the query points to no country.
Each labelling is kept once, under the country that scores it best.
Postcode shape alone does not pick the country, since five digits such as 72100 are valid in DE, FR, US, IT and ES.

Rejected.

- **Also score against a global reading.** A global reading leaves the country's evidence out.

Evidence.

72100 matches DE, FR, US, IT and ES but not CA.

### A written number sign makes the number a flat or suite, never the house

Where. `after_number_sign` and `is_number_in_parts` in `src/parse/lattice.rs`.

A number written after "#" and after some other word ("District Ave #300") numbers a flat or suite, never the house, even though the search key drops the sign and leaves a bare number.
Numbers written in parts ("993.5", "1384 1/2") are one house number, joined by dot or slash.
Commas are a feature of the parse, never its rule.

Rejected.

- **Rely on the search key alone to read a number.** It drops the # sign so a suite number would look like a house number.

### Admin aliases and numbered forms are only trusted where they are unambiguous

Where. `finish`, `MIN_NUMBERED_NAMES` and `admin_types` in `src/parse/lattice/dictionary.rs`.

An alias of admin areas without their type word joins the names of one field, unless it is already the name of an area of another field or names areas of two ('京都' of '京都府' and '京都市' is neither).
Levels of one field count as one label, so 'ōta' of the ward 'ōta ku' and of the city 'ōta shi' is a locality either way, and the rest of the address tells the places apart.
Names that only records state ('addr:city=東京') are no areas and are often the very area the alias names.
A numbered form shared by at least 5 area names (MIN_NUMBERED_NAMES) is a kind of area ('#丁目'), not one name with a numeral in it ('四日市市'), so '5丁目' is still a place of a kind when the Pack lacks it by number.
A name that also names a neighbourhood or block is dropped from the place lists, because the Pack holds every country, region, district and city but neighbourhoods and blocks only here and there ('中島町' of one city, '5丁目' of hundreds), so the address check cannot hold such a name to the rest of an address.
A libpostal qualifier of an unspaced script or Hangul attaches after the name.
Any other is a word after the name and never before it, because a word before a name cannot be told from the end of the field before it ('2 chome taitoku' is no 'taitoku' chome).

Rejected.

- **Treat a word before a name as its type.** It cannot be told from the end of the previous field, so 'chome' in '2 chome taitoku' would wrongly type 'taitoku'.
- **Check addresses against names that also name neighbourhoods or blocks.** The Pack does not hold those levels completely, so the check would wrongly reject real addresses.

Evidence.

Examples from the comments are 京都 (京都府 and 京都市), ōta (ward ōta ku and city ōta shi), 中島町 of one city versus 5丁目 of hundreds, and 2 chome taitoku.

### Glued street types need a stem of three letters

Where. `MIN_GLUED_STEM_CHARS`, `Abbreviations::split_glued` and `Abbreviations::unglued` in `src/parse/lattice/abbreviations.rs`.

Abbreviations are read both ways, so a query's str also looks up straße and training writes straße as str some of the time.
In languages that glue the street type onto the name, a glued type is read off a word only when the stem keeps at least 3 characters, so hauptstr counts and estr does not.
Phrases of several words are left out because they are written as words of their own.
No code names a language, so every list is read the same way.

Rejected.

- **Read a glued type off any word ending in a variant.** A short stem like estr would be taken for a street type.

Evidence.

hauptstr versus estr.

### Hand transliteration tables for kana, Cyrillic and Greek

Where. `to_latin` in `src/util/transliterate.rs`.

Latin forms of names in kana, Cyrillic and Greek come from small hand tables read sound by sound.
Kana use Hepburn with long vowels written short as signs and maps write them (とうきょう is tokyo).
Han characters have no one reading, so a name holding any is left alone, as is a name of any script without a table.
The result is folded to ASCII like every other Latin name.

Rejected.

- **CLDR script-to-Latin transforms.** They write scholarly forms nobody types (selkovo for щёлково, athḗna, toukyou), read Japanese kanji as Mandarin, and have no Thai or any-script form.

Evidence.

selkovo for щёлково, athḗna, toukyou, and kanji read as Mandarin.

### Lattice parser reads the span as written before other spellings and limits fuzzy matching

Where. `Dictionaries::lookup`, `MIN_FUZZY_CHARS`, `MIN_FUZZY_POI_CHARS`, `SpanKey::dfa` and `is_unspaced` in `src/parse/lattice.rs`.

A span is looked up as written first, and another spelling of it (the name without its type word, an abbreviation in full) is read only when the span names nothing, so "verwaltungsgemeinschaft stockach" is the district it names, not the city "stockach".
Fuzzy lookup is skipped for spans under 4 characters because one edit turns most two- and three-letter words into many others, and for a one-token POI span under 6 characters because POI names are many and a short word is one edit from many venues.
A span with a digit is never corrected, a span of all known words is no typo, and scripts written without spaces are never fuzzy because every character is a word so one edit names another place rather than a typo of this one.
Fuzzy names must start with the key's first character, which typing errors seldom change.
Scripts without spaces are split to one unit per character so "東京都千代田区" reads the same with spaces between its names, without them, or with a space after every character, and unit detection uses Unicode Script_Extensions so the prolonged sound mark "ー" and iteration mark "々" count while a digit does not.

Rejected.

- **Try all spellings of a span with equal priority.** "verwaltungsgemeinschaft stockach" would resolve to the city "stockach" instead of the district.
- **Fuzzy lookup on short spans, POIs of one short word, or unspaced scripts.** One edit reaches many other names or places.

### Parser benchmark aligns characters and generates fixed variants

Where. `align_components` in `src/parse/bench/align.rs`; `Rng` and `messy` in `src/parse/bench/variant.rs`.

A baseline's output is aligned to the gold tokens character by character with a longest common subsequence, so a parser that splits, joins or drops punctuation differently is still scored on og's tokens.
A lowercased Turkish İ is i plus a combining dot, an accent and never a word break.
Messy variants use SplitMix64 seeded from a fixed seed and the row's own text, so the same row is perturbed the same way wherever it sits in the file and the variants never change with a dependency.

Rejected.

- **Compare tokens directly.** A baseline that glued 26 th or dropped york would not line up with og's tokens.
- **A random number crate for the perturbations.** A dependency update could change the generated variants.

Evidence.

A test has a baseline that glued 26 th and dropped york.

### Parser training uses 200,000 rows per country

Where. `ROWS_PER_COUNTRY` in `src/parse/lattice/train.rs`.

ROWS_PER_COUNTRY is 200,000 training rows for each country of the Pack.
This was measured on the Japan Pack, where 200,000 rows label the held-out rows and the libpostal bench as well as 400,000 rows do.
A budget of 100,000 rows labelled both worse.

Rejected.

- **100,000 rows per country (the figure Deepparse trains on).** Labelled worse than 200,000 on the Japan Pack, on held-out rows and on the libpostal bench.
- **400,000 rows per country.** No better than 200,000 on the same checks, so it only adds cost.

Evidence.

Japan Pack.
100,000 rows was worse, 200,000 matched 400,000, on held-out rows and the libpostal bench.

### Rows with no country are skipped when the Pack has countries

Where. `train` in `src/parse/lattice/train.rs`.

Records that lie in no country stand for a country only in a Pack that has none, such as a city extract cut inside its country's boundary.
In a Pack that has countries they are only the strip an extract takes past a border, so their drawn rows are skipped.

Rejected.

- **Train on the no-country records beside the country ones.** They would take a whole country's share of rows, though they are only a border strip.

Evidence.

The skip is the in_countries check, which continues past the None country.

### Street type abbreviations expand but St before a number or at the start is left as Saint

Where. `expand_address_abbreviations` in `src/pack/text_index.rs`.

Indexed text and queries expand street type abbreviations and directions that follow a street type alike, so either spelling finds the other ("king st w" becomes "king street west").
An initial or post-number "st" is left alone because there it is usually Saint ("St Clair Avenue", "12 St George Street").
An English ordinal written out before a street type becomes its number ("fifth avenue" is "5th avenue", "twenty first street" is "21st street") but "first baptist church" keeps its word.
The ordinal check runs after expansion so "fifth ave" finds its type too.

Rejected.

- **Always expand st to street.** At the start or after a number it is usually Saint.
- **Convert every written ordinal to digits.** "first baptist church" would lose its word.

### Training shard count is fixed at 4, not taken from the cores

Where. `TRAIN_SHARDS` and `MIN_TRAIN_ROWS` in `src/parse/lattice/train.rs`.

Training runs a fixed 4 shards side by side and mixes their weights each epoch.
More shards see fewer rows between two mixes and converge slower.
The count is fixed instead of read from the core count so a build trains the same weights on every machine.
A Pack that renders fewer than 1,000 rows skips training and parses with the built-in hand-set weights, because too few rows teach a model less than those weights already know.

Rejected.

- **Shard count equal to the number of cores.** The weights would differ from machine to machine, and more shards converge slower.
- **Train on any Pack however small.** Under 1,000 rows the trained model knows less than the hand-set built-in weights.

Evidence.

TRAIN_SHARDS is 4 and MIN_TRAIN_ROWS is 1,000.

## Search

### Famous-place searches read a copy of the famed documents first

Where. `TextSearcher::prepare_famed`, `TextSearcher::famous_as`, `TextSearcher::expansion_end` and `Corpus` in `src/search/mod.rs`, `PhrasePrefix::ending_at` in `src/search/phrase_prefix.rs`, and the `famed` thread in `AppState::open` in `src/http/server.rs`.

At server start a background thread copies the documents whose fame is above 0 into an in-memory index, 2,227,162 of the planet's 677,624,140.
A famous-place search reads the copy first, scored with the whole index's statistics.
A prefix expands in the copy to the same last word it expands to in the whole index.
When the copy fills the rank window, the whole index would fill it with the same documents, since fame sorts first, so the search stops there.
When it does not, the search reads the whole index, as before the copy existed.
The copy is made only when the text index is one segment, and until it is ready every search reads the whole index.

Rejected.

- **Read only the copy.** Chain stores carry fame only on their brand, so every Tim Hortons store has fame 0, and `Tim Ho` near Toronto lost them to Bangladesh.
- **Let the copy expand prefixes from its own words.** Its dictionary is smaller, so the first 1,024 words after `st` reach further, and it found places the whole index never reached.
- **A famed filter clause on the whole index.** The postings of every expanded word are still walked, so it was no faster.
- **Run the famous searches on parallel threads.** One request got faster, but at 32 requests at once throughput fell from 113 to 92 per second, as the threads queued for the shared pool.
- **A hand-built table of famous names.** It would have to repeat tokenising, other word forms, typos and the context field, and drift from the index.

Evidence.

Every famous-place search that read the copy over 2,904 keystrokes returned the same documents as the whole index, to the last digit of rounding in the score.

### Prefix completion lists are kept per field and prefix

Where. `TextSearcher::common_completions`, `TextSearcher::scan_completions` and `COMPLETIONS_KEPT` in `src/search/mod.rs`.

The completions of a prefix are read from the term dictionary once and kept, since the index never changes while the server runs.
At COMPLETIONS_KEPT prefixes the store is emptied and starts again.

Rejected.

- **Keep whole answers.** Each stage of a search has a 300 ms budget, so under load an answer can come back cut short, and keeping it would serve the cut answer to everyone after.

### A flat or unit scores nothing and only breaks ties

Where. `TextSearcher::house_number_query`, `QueryPlan::from_tagged` and `TieBreak` in `src/search/mod.rs`.

A named unit finds its flat as written ("29/181") and scores what the house number does, never more.
As a plan field it scores nothing and chooses among the addresses of one street in the tie-break.
The unit is the one the plan read, never a word of the house number ("7 Vialou Street" is no unit 7).

Rejected.

- **Score the unit as ordinary query words.** A flat's number is rarer than the house's, so its own score would rank a street with many flats above the street the query names.
  "Unit 2, 1 Balfour Street" was answered on a Balfour Road with a 2/1.

### A geocoded row refuses what search merely ranks down

Where. `TextSearcher::geocode` and `TextSearcher::address_with_unconfirmed_neighbourhood` in `src/search/mod.rs`.

Geocoding a structured row is stricter than search.
A hit outside the row's stated postcode is no answer, where in search it only ranks down.
A street standing in for a missing number must have a given postcode confirmed by postcode data, not merely uncontradicted.
A stated area must be confirmed and a name the Pack has never heard of neither confirms nor rejects.
A parse with no house number ("997" read as a unit) is not given as an exact house, because search would answer it with the street group's first address, a house the query never named.
A weaker parse that reads the number as a unit or a word is refused for the same reason.

Rejected.

- **Accept the street stand-in as search does.** The query fell through to parses that matched another street, "10 Eruera Street" to "10 Kevin Street".

### A street standing in for a number is placed on its line, not its centroid

Where. `TextSearcher::place_on_street_line` and `TextSearcher::place_between_neighbours` in `src/search/mod.rs`.

A street answering for a missing house number is moved to the point halfway along its line by distance.
The centroid is a fine label anchor but can sit off the street.
A point along the line is on the street by construction.
Halfway is measured by distance, not vertex count, so a densely drawn bend does not pull it in.
A number with no record or range is placed between stated neighbours of its own parity, since odd and even numbers face each other on most streets.
Both neighbours and the point between them must lie on a street of that name, so two streets sharing a name in one locality ("Victoria Street") never pair.
The postcode the query names wins, then the closest numbers.

Rejected.

- **Use the street's centroid.** The centroid of an L-shaped or crescent road falls in the space the bend encloses, and on a long road it can be a kilometre from the pavement.

### Areas may stand in for some required words, never all

Where. `TextSearcher::subject_clauses` and `TextSearcher::build_query` in `src/search/mod.rs`.

Words of an area the query states are searched there already, so the subject does not let that area absorb its own copy of a word.
Areas may answer for some required subject words but never all of them.
A word that only describes a street ("road") names nothing and cannot meet the required floor.
Region, country and postcode parts only rank and never filter, because few records state the "ON" a query may write and a record's tags often state a different postcode than the one written.

Rejected.

- **Let areas answer for every required word.** Any street in Mount Eden would answer "10 Eden Road Mount Eden" read as one street name.
- **Count describing words toward the required floor.** Any road in Christchurch would meet the floor for "Ponsonby Road, Christchurch".

### Common-word completion scan is capped

Where. `MAX_COMPLETION_SCAN` and `TextSearcher::common_completions` in `src/search/mod.rs`.

Completing a typed prefix by the most common words reads the prefix terms only up to MAX_COMPLETION_SCAN and gives up past that.
A one letter prefix such as "s" then stands only for "south".
Index order stops at the expansion cap before common words on a planet Pack, so "mar" would never reach "market" without the common-word path.

Rejected.

- **Read every word of the prefix on every keystroke.** The prefix scan is paid per search per keystroke and is too slow for short prefixes.

Evidence.

A planet Pack holds 33,000 words of "mar" (7 ms to read) and 606,000 of "s" (210 ms).

### Confidence and ranker models are sidecar files, not Pack sections

Where. `Model` in `src/search/confidence.rs`; `Ranker` in `src/search/rank.rs`.

The calibrated confidence model and the learned ranker are sidecar files beside the Pack file.
A published Pack is immutable and mapped by the servers reading it, and a planet Pack is too large to copy for a few hundred bytes.
The model is fit after the build, from data the build never sees, and may be refit at will.
Each file names its Pack by fingerprint, and a Pack refuses a model fit to another, because a rebuilt Pack ranks differently.
A Pack with no fitted model gives no confidence at all, not a made-up one.

Rejected.

- **Store the model as a Pack section.** The Pack is immutable and mapped by live servers, a planet Pack is too large to copy for a few hundred bytes, and the fit happens after the build.
- **A default confidence for Packs without a model.** A made-up confidence would not be a calibrated probability.

Evidence.

The sidecar is named pack.confidence.json for pack.ogp, and the ranker is a sidecar in the same way.
The Pack match is checked by Container::fingerprint.

### House numbers and short words are never typos

Where. `allowed_edits` and `MIN_TYPO_CHARS` in `src/search/mod.rs`.

Words shorter than MIN_TYPO_CHARS and any word holding a digit are matched only as written.
A house number, postcode, unit or route number is never a typo.
Words of four to six characters allow one edit and longer ones two, and each replacement character from a wrong encoding counts on top.

Rejected.

- **Typo tolerance on numbers.** In a dense run of numbers every edit names another real address, so "12 Main St" is one edit from "13 Main St".
  One edit also turns most words of two or three letters into many others.

### Named-place filtering relaxes by level and a region never holds a candidate

Where. `levels`, `named_places` and `written_country` in `src/search/named.rs`.

Candidates are held to the strictest level any meets, strictest first.
Postcode and town together, then when both are named the postcode alone and the town alone, since either may be written as the post office names it, then the country alone.
A region only chooses the country because a region's words also name small places ("Württemberg").
The country is the one the query writes even if the reading takes its words as something else, so "Saint-Dizier 52100 FR" still names France.
Countries score one for the stated country and one per named place they hold, so a postcode shared by two countries ("02130" of France and Boston) is read in the country the rest agree on, and a stated country that postcode and town both disagree with ("Duisburg 47051 France") gives way.
A placeholder postcode ("00000" in Hong Kong) names nothing.
When no candidate lies even in the named country the query is answered by the finest area it names, never by a hit in another country.

Rejected.

- **Hold candidates to the region the query names.** Region words also name small places such as Württemberg.
- **Require postcode and town together always.** Either may be written the way the post office names it.

### Neighbour search radii fit lots and divided roads

Where. `NEIGHBOUR_STREET_RADIUS_M`, `ROAD_SETBACK_M` and `STREET_DISK_M` in `src/search/mod.rs`.

An address point may lie 100 m from its street, which covers the depth of nearly every lot including large buildings behind parking, while a point between addresses on two streets of one name lies far from both.
A neighbour on a divided road may lie 200 m from the road line, since the line runs down the median and lots along either carriageway sit further.
A street answer sits at the centre of the densest 100 m disk of its address points, since an answer within 100 m is the goal.
Two pieces of one road are one answer, not a tie between two.

Evidence.

Lots sit 100 to 200 m from the line on the widest divided roads.

### One Pack over several countries, filter narrows without ranking

Where. `build` in `src/builder/osm.rs`; `parse_countries` in `src/http/server.rs`; `TextSearcher::countries_query` and `Filters` in `src/search/mod.rs`.

One Pack covers several extracts.
The four PBF passes run once per input because the node pass stores coordinates in id order, which only holds within one sorted PBF.
All records land in the same sorters, so boundaries, record write, seal and parser training run once over everything.
That keeps one text index with one set of collection statistics, so BM25 scores stay comparable across regions.
The country filter narrows and does not rank, so a hit keeps the score it had unfiltered.
Country and other filters are exact terms and ranges wrapped in a constant-score query at zero.
They narrow what is answered and never rank it.
The index and its collection statistics stay whole, so scores stay comparable across the countries in one Pack.

Rejected.

- **A Runtime per country behind a router.** The scores being merged come from different indexes and are not comparable at all.
- **A plain term query for the country filter.** It would add its own BM25 score to every hit, and that score depends on how many records the country holds, so the same query would rank differently filtered than unfiltered.
  That brings the cross-index score comparison problem back inside one index.

### Per-country confidence models are shrunk toward a pooled model

Where. `COUNTRY_PRIOR`, `POOLED_RIDGE` and `fit_logistic` in `src/search/confidence.rs`.

One pooled logistic model is fit over every country, and each country's model is held toward it by a prior of strength 10 per feature.
At an even chance an example adds a quarter to the curvature, so a few dozen examples are needed to move a weight, and a country with a handful of examples keeps the pooled model.
The pooled model has a ridge of 1 per feature so a feature the data never varies, or one that separates the examples perfectly, keeps a finite weight.
The bias is left free.

Rejected.

- **Fit each country on its own.** A country with a handful of examples would get unstable weights.

Evidence.

COUNTRY_PRIOR is 10.0 and POOLED_RIDGE is 1.0.
A Hessian diagonal of strength plus 1e-9 keeps the system solvable when nothing holds a weight.

### Places used in more than 4096 cells keep only an extent

Where. `PLACE_CELLS` and `direction_spellings` in `src/search/structured.rs`.

A place used in more than PLACE_CELLS (4096) grid cells, such as a region or a large city, stores only its bounding extent.
A cell list that long filters little that a box does not, and costs each query more to read.
Separately, direction_spellings adds or removes a direction word at either end of a street name, but a name of one word besides the direction keeps it, because 'north street' is a name, not a direction.

Rejected.

- **Store every cell of every place.** A list that long filters little a box does not and costs a query more to read.

### Ranker and confidence model apply only to numbered queries

Where. `Numbered`, `TextSearcher::rate` and `TextSearcher::search` in `src/search/mod.rs`.

The calibrated ranker and the confidence model are fitted to queries that state a house number, so they order and rate only those.
A query for a town or a street keeps the order its own words give, and a fallback is never rated by the model.

Rejected.

- **Apply the ranker to every query.** It was fitted only on numbered addresses, so its order and ratings would be unfounded elsewhere.

### Road names join by words, with a 1,000 m link and a 3 letter glued type

Where. `ROAD_LINK_M`, `MIN_GLUED_TYPE`, `StreetParts::of` and `RoadName::same_road` in `src/search/road.rs`.

Two pieces of one road join when the names match and they lie within 1,000 m, which is farther than the gaps between address points along most roads and nearer than two roads of one name in one town usually are.
A street type glued onto a name is split off only when at least 3 letters of it are glued, because the tables list one-letter endings that end many names.
A word is split off a street name only while a word that names the street is left, so North Street is the name North and the type Street, never a direction and a type alone.
A short type form may stand for more than one word, as st stands for street and saint.
A direction only one name states does not tell two names apart, but two different directions do.

Rejected.

- **Split off every glued type ending.** One-letter endings end many names that are not types.
- **Always peel direction and type words.** North Street would be left with a direction and a type and no name.

Evidence.

Examples are Roosevelt Boulevard against E ROOSEVELT BLVD, Pražská třída against Pražská tř., and hauptstr read as haupt str.
The link distance is ROAD_LINK_M = 1_000.0.

### Search budget cuts the least likely readings first

Where. `SEARCH_BUDGET`, `MAX_SEARCHES` and `Session` in `src/search/mod.rs`.

A query searches for at most 300 ms and then answers with the best it found.
Readings run best first, so the budget cuts off the least likely to answer, and the tail of slow queries ends there.
The first search always runs so every query is searched.
Tests use an hour and are bounded only by MAX_SEARCHES, because a clock would make answers depend on machine load.

Rejected.

- **A clock in tests.** Tests run unoptimised on a loaded machine, so their answers would depend on it.

### Search keys use the Latin-ASCII table, and exact keys keep accents

Where. `search_key`, `exact_key`, `split` and `replaced_characters` in `src/util/fold.rs`.

A search key is NFKC, lower case and Latin letters written in ASCII by the CLDR Latin-ASCII table (lodz for łódź, strasse for straße, orsted for ørsted).
Decomposing and dropping marks alone would leave ł, ø, ß, æ, đ and ı as they are because they carry no mark.
The exact key keeps accents (NFC, lower case), so a query spelling a name exactly ranks it above a plainer namesake (Taupō over Taupo).
Words are runs of letters and digits, and a mark continues the word before it, so a decomposed wa+macron+naka is one word.
A replacement character (a letter a wrong encoding lost, as in Ch?teauneuf for Châteauneuf) stays inside its word, one edit from the letter, so a typo tier can reach it, and a run of only replacement characters is a gap.
Conjoining jamo compose only across letters, so a word written with them has no key a query can equal.

Rejected.

- **Decompose and strip combining marks only.** Letters such as ł, ø, ß, æ, đ and ı carry no mark and would stay unfolded.
- **Drop the replacement character or treat it as a word break.** It would cut Châteauneuf in two instead of staying one edit from the real spelling.

Evidence.

lodz, strasse, orsted, Taupō versus Taupo, Châteauneuf with a replacement character.

### Stated places and names are read by whole names and nesting

Where. `TextSearcher::stated_names`, `TextSearcher::area_agreement`, `TextSearcher::near_settlements`, `TextSearcher::postal_town` and `TextSearcher::areas_of` in `src/search/mod.rs`.

A locality part is read as the names in it, longest first.
Names that lie one in another ("Mount Eden Auckland") must all agree.
Names that cannot hold one address (Serfaus, Fiss and Ladis are neighbours) mean the query names its area more than once, so one agreeing will do.
Only the whole name of an area a hit lies in is its own, so "Warwick" is not West Warwick.
A locality agrees through a postal town, found from the Pack's own addresses so no table of postal towns is needed ("Alexandria, VA 22310" is a postal town over part of Fairfax County).
It also agrees through a settlement mapped only as a point among the few nearest, for towns with no boundary.
A point inside a boundary of its own name names nothing past it.
A district such as Providence County names a town only for a record no town-tier area holds, since its name is often one of its towns (Woonsocket).

Rejected.

- **Treat any name word as the area.** Dayton's node would then be a sign that Clayton is Dayton, and the name of a county that holds many towns would stand for one of them.

Evidence.

Bootle is a town with no boundary of its own, and its streets lie only in Sefton's.
Seaforth in Huron East and Grey Lynn in Auckland have no boundary but an address can state them.

### Street type and direction mismatches rank down, never out

Where. `TextSearcher::street_contradicts`, `TextSearcher::descriptors`, `TextSearcher::street_keys` and `SubjectWord` in `src/search/mod.rs`.

A street with every word of the stated name but a different type or direction ranks after the streets that agree ("South US Highway 281" for "North US Highway 281", "16th Avenue North" for "16th St N").
It is never rejected, because a query may write a type wrong.
A street that says nothing of a kind contradicts nothing, and nor does one of another name, since a POI's name word may be a direction somewhere ("Tim" is east in Indonesian).
A type glued onto a name counts as a type ("plein" of "wilderinksplein") and is left out of the key, so "Hauptstr", "Hauptstraße" and "Haupt Straße" key as one.
But two streets differing only by the glued type stay two, and the one of the type written answers.
Names of fewer than three letters take no glued type ("16th").
A word that is both a type and a name ("st", "dr") cannot be read one way, since "Main St" is "Main Street" and "St Johns" is "Saint Johns", so a street has a second key and record and query are keyed both ways.
Street type abbreviations are searched as extra full forms and not rewritten in the indexed text, since they are too ambiguous across languages ("pl" is "place" and "platz").

Rejected.

- **Reject streets whose type or direction differs.** A query may write a street's type wrong, so a hard rejection would lose the right street.
- **Rewrite abbreviations in the indexed text.** Abbreviation lists are ambiguous across languages ("pl" is "place" and "platz"), so the query asks for each form instead.

### Structured stage answers only when hand-off models agree

Where. `TextSearcher::structured_stage`, `TextSearcher::verified`, `TextSearcher::in_stated_towns`, `STRUCTURED_W`, `STRUCTURED_DELTA` and `STRUCTURED_MIN_FORM` in `src/search/mod.rs`.

The structured index answers an address or street before any text search only when two fitted models say it is likelier within 100 m than the text search's answer by STRUCTURED_DELTA.
STRUCTURED_W, STRUCTURED_DELTA and STRUCTURED_MIN_FORM were chosen together so the stage answers as often as it can while its answers are as often within 100 m and 600 m as the text search's and seldom in a town the query does not name.
Coarser types (a street, a postcode, a town, a region alone) are left to the text search.
A business is left to the text search, which weighs it by name.
A stated postcode the hit agrees with outweighs a town it does not, since a query may name a postal or neighbouring town, but the town still costs score.
A street the stated town lacks costs the parser's score and is not a veto, and outside the towns the query states an answer is never given.
A street so common that more than STRUCTURED_CANDIDATES documents match needs the text search's ranking.

Rejected.

- **Always let the structured stage answer.** Its answers would sometimes be farther than 100 m or in a town the query did not name.

### Typed fragments favour what is near the caller

Where. `mix_suggestions` and `TextSearcher::suggest` in `src/search/mod.rs`.

Autocomplete orders by the query because scores over different fields do not compare.
A leading house number puts addresses first.
Otherwise a place whose name starts as the query comes first, then addresses, then other places.
A part typed in part ranks what lies within the caller's reach before what lies beyond, so "rest" by a Baltimore restaurant is the restaurant, not a rest area a state away.
An incomplete address does not trigger a geocoding retry on each keystroke.

Rejected.

- **Compare scores across fields.** Scores over different fields do not compare.

### Typo automata cache is bounded in states

Where. `MAX_CACHED_AUTOMATON_STATES` and `TypoAutomata` in `src/search/mod.rs`.

Each thread caches Levenshtein automata for the query words it saw last, limited to 16 KiB states (16 MiB a thread).
That still holds every word of a query and those of the queries just before.
Ranking asks every query word against every word of every hit and building a two-edit automaton costs far more than running one, so the cache stays.

Rejected.

- **Cache up to 4,096 automata per thread.** A state is a 1 KiB table and a ten-letter two-edit automaton has about 250, so that came to a gigabyte a thread.
  Lookups run on blocking threads that come and go, so each new thread filled one more.

Evidence.

On 16 CPUs under load the server ran out of memory and refused nearly every request until it was restarted.

### Typo handling scores a corrected word between written and neighbour

Where. `CORRECTED_BOOST`, `TextSearcher::near_terms` and `TYPO_PREFIX_CHARS` in `src/search/mod.rs`.

The word the parser read a typo as counts for less than a word as written and for more than the other terms near the typo, which count nothing.
Typos are folded into the one search of a query so no search runs again to allow one.
Only index terms more common than the written word are kept as neighbours, because a typo turns a common word into a rare one far more often than the reverse.
"maine" may be a typo of "main", but "main" is not a typo of "maine", and a common word keeps no neighbours.
One leading character must be written as typed.

Rejected.

- **Let each fuzzy neighbour score by its own BM25.** Tantivy's fuzzy query scores every term it matches the same, and a term's own BM25 would rank a rare neighbour of a word over the common word itself.
- **Keep two leading characters as written.** A swap of the second and third letters ("Mcihael") is the commonest slip, one edit with transpositions counted as one, and two fixed characters would rule it out.
  One fixed character still keeps the dictionary walk to one branch.

## Ranking

### Words in any order match within one name and the areas it lies in

Where. The `Completion::AllWords` stage of `TextSearcher::famous_places` in `src/search/mod.rs`.

A famous place found with the typed words in any order is kept when one of its names takes the first typed word, and every other typed word starts a word of that name, of its label or of an area it lies in.

Rejected.

- **Any typed word in any of the place's names.** A place carries names in many languages, so `Tim Ho` matched Bangladesh by `tim` in the Malay `Pakistan Timur` and `ho` in the Vietnamese `Cộng hoà`, and Bangladesh outranked every Tim Hortons.

Evidence.

`Tim Ho` gives Tim Hortons Iceplex and Tim Ho Wan, not Bangladesh, `den ha` gives Den Haag, not Algeria, and `IV, To` gives the lake IV.-tó, not Iran.

### Answer choice reorders only on house number on the road

Where. `choose` and `agreement` in `src/search/choose.rs`.

Hits carrying the house number on the road go first and the rest keep their order.
The road, postcode and town alone reorder nothing.
The number and road count only inside the query's places, so the same number on another road, or on a road of the name in another town, is no better than any other answer.

Rejected.

- **Also promote hits that agree on road, postcode or town.** An address whose locality is its postal town, not the town the query names, would drop below the street.

### Caller focus ranks in widening bands within 50 km

Where. `FOCUS_BANDS_M`, `BIAS_BANDS`, `RankKey` and `TieBreak` in `src/search/mod.rs`.

Nearness is a band that widens with distance, because a town 30 km away is about as near as one 40 km away while 1 km and 10 km are not.
Bands within 50 km, the reach of a map view's bias, rank before text score, so "starbucks ballard" in Seattle is not the one on Ballard Road in Oklahoma, and "tacoma" from Seattle is the city, not Tacoma Screw down the road.
Beyond 50 km a better match wins, so "paris" from Seattle is France.
In the tie-break focus comes after every query word, so a caller's location never outranks a better match.
Fame comes before place size (Salzburg the city before the state, Paris in France before Paris in Benin).
A size prior only decides ties on the words and areas a query states.
A POI's prominence comes after nearness, else the window would hold the best known of a chain's thousand equal names and the ones near the caller would never make it.

Rejected.

- **Rank by exact distance or let focus outrank the match.** Exact distance splits places that are about as near, and a location bias that beats a better match answers the wrong place.

### Evidence is compared in order, never summed

Where. `Evidence`, `TextSearcher::search_variants` and `TextSearcher::search_plan` in `src/search/mod.rs`.

A hit rests on evidence compared in a fixed order.
First fewer typos, then a stricter pass, then a confirmed postcode, then the parser's score.
Fewer typos outweigh any pass, a stricter pass outweighs any score, and the score decides only between hits found alike.
A hit whose own postcode or street contradicts the query joins the pool but rests on no evidence, so it cannot end the search.
A number read as a postcode that nothing confirms may be the house number, so the next reading still runs.

Rejected.

- **Sum a retrieval score with the parser score into one number.** The parser score is an unnormalised Viterbi path score on no scale shared with retrieval, so any joining weight would be made up.
- **Let a stated house number in the wrong postcode win the strict pass alone.** The later passes that would have found the right street never ran.
  "8 High Street, Deloraine TAS 7304" would be answered by the 7018 High Street 163 km away just because it states the number, and "3010 Coast Road, Barrytown" is the house 3010, not any house in a postcode 3010 no Barrytown record states.

Evidence.

A test records that a stated number in the wrong postcode used to win the strict pass and return alone.

### Name-only answers use ordered keys, partial names never place

Where. `order_by_names`, `order_key` and `PlacedBy` in `src/search/names.rs`.

A query with no house number is split into parts at commas (except where the comma is part of a recorded locality name).
Each part names a record (whole, or whole bar a type word, then misspelt, then in part) or places it (by an area that holds it, or a longer area name).
A part is where when a record it names holds a record another part names, and the first part left is sought.
A name written only in part says nothing of where.
Answers are ordered by keys compared in turn and never summed, and how the sought part names a record counts before how the other parts place it.
A record the sought part does not name comes after every one it does.
A part that cannot tell whether an area holds the record (a village past an extract border, a record whose country no boundary gives) neither places it nor leaves it out.

Rejected.

- **Sum the keys into one score.** Each key is compared in turn and none is summed with another, so a strong placing cannot outvote a weak naming.
- **Let a partial name say where.** Washington would make Kirkland where for holding Lake Washington Boulevard.

Evidence.

Exeter, RI asks for Exeter, which Rhode Island holds.
Cranston Street, Cranston asks for the street, which Cranston holds.
Cranston, Providence asks for the town Providence County holds, not Cranston Street in the city, while Cranston Street, Providence asks for the street in the city over the one in the county.
Town of Bristol names Bristol and Mooresfield names Mooresfield Road.
Providence places East Providence by a longer name.

### Place name lookup collects by fame, not text score

Where. `TextSearcher::places_by_name_keeping`, `TextSearcher::by_name` and `FameKeys` in `src/search/mod.rs`.

The candidate window for a place name is collected nearest the caller first, then best known, whatever the text score.
Exact name, own or alternate, comes before a name that only holds the words.
Then nearer the caller, so fame never outranks the caller's own surroundings, then better known.
Of places about as well read, the one the query names by its own name wins, so "Lagos" is the city, not Laos.

Rejected.

- **Collect the window by BM25 text score.** BM25 scores a name field down for its length, and the best known places carry the most names (Roma's own and its "Rome", "Rom", "Rzym"), so by text score every small Rome would fill the window first.

### Postcode agreement weights are counted per country

Where. `PostcodeWeights`, `MATCH_AGREEMENT` and `TextSearcher::postcode_weights` in `src/search/mod.rs`.

Agreeing on a postcode weighs ln(m / u).
Here u is the share of a country's addresses stating that postcode, counted per country from the Pack's own counts. m is one shared constant, 0.95, since no answer key exists to measure it per part, so the order rests on u alone.
The log is natural so the weight adds to BM25's idf on one scale.
A postcode most addresses state weighs next to nothing and never counts against a record.
A record in a country is weighed by that country's counts, never by postcodes no country holds.

Rejected.

- **One global postcode weight.** It cannot fit both a UK postcode, a street's side, and a US ZIP, thousands of addresses.
- **Counts over the whole Pack.** A code names a postcode only within one country (AU and NZ both have a "2110"), and a Pack covering more countries would make every postcode rarer.
- **Log base 2.** BM25's idf is itself a natural log of a rarity, so natural log keeps one scale.

### Rank window collects by text score and widens on ties

Where. `TextSearcher::ranked_hits`, `TIE_WINDOW` and `RankKey` in `src/search/mod.rs`.

Documents are collected by text score alone so the index can skip those that cannot make the window, and only the window is keyed.
A house number, range and postcode are scored in the query so what the key ranks first scores first.
When the window ends inside a run of equal scores, the run is collected whole up to TIE_WINDOW so the rest of the key can choose, such as "Richmond" the town among a hundred Richmond Roads.
A run past the tie window, or a near-first search, keys every match, paid by such runs alone.

Rejected.

- **Take the first window by score and cut.** Which tied documents made it is arbitrary, so "Paris" or the Main Street near the caller could be missed.

### Words standing in for an area score a share of idf, not their own area BM25

Where. `TextSearcher::stand_in_weights` and `AREA_STANDS_IN_BOOST` in `src/search/mod.rs`.

A word that may name an area, where the query names none, scores AREA_STANDS_IN_BOOST (half) of its idf in the field a street is searched in, or in where records are when that is lower.
Half of the idf ranks the stand-in below a word asked for where it belongs.
"7 Lynn Place" is then the street, not a place in the New Lynn suburb.

Rejected.

- **Use the word's own BM25 in the where-a-record-is field.** "road" is rare among area names and common among streets, so "47 Mill Road" would find a Millton Park Road in a locality named Mill Road before Mill Road itself.

## Building a Pack

### A label node's Wikidata item is not trusted to name the boundary

Where. `finish_place_merges` in `src/builder/osm/boundary.rs`.

A boundary keeps its own Wikidata item.
The item of a merged place point fills in only when the boundary has none.
The strongest link wins, so a label node beats a name or Wikidata match.
Matching happens for all points before any merge, so no result depends on arrival order.

Rejected.

- **Always take the label node's item.** A label node's item often names something else, such as the settlement inside a commune, the district a cadastral quarter shares its name with, or a historical region.

### A stopped build is never deleted unasked, and an incompatible checkpoint is left for resume

Where. `find` and `refusal` in `src/builder/resume.rs`.

A new build that finds a stopped build asked neither to resume nor to start over refuses and says what was left and how to do either.
Find does not clear a checkpoint just because it is incompatible.
It lets the checkpoint reach resume's version check, because clearing it as unfinished would discard the saved build on --resume.

Rejected.

- **Delete or overwrite the leftover build automatically.** It would delete hours of work or fill the disk twice.
- **Clear an incompatible checkpoint as unfinished.** It would discard the saved build when the user passed --resume.

### Admin levels get layers from tags and gap filling, per country

Where. `assign_layers_by_containment`, `tagged_level_layers` and `conventional_admin_layer` in `src/builder/osm/boundary.rs`.

Every boundary gets a layer per country and admin level.
A level whose boundaries say what they are wins first.
When at least 10 boundaries of one level and country carry a place tag and at least 80 percent of them name the same layer, the whole level takes it.
Levels the tags say nothing about keep the conventional layer for their number.
Off-convention levels move into whichever layers the country leaves empty.
This needs no per-country table to maintain, so a country nobody has looked at still gets a locality layer.
Gaps are counted per country because a level means different things on either side of a border.
They are not counted per chain, because one sparsely mapped region would then classify differently from its neighbours, which is how a relative-depth scheme goes wrong.
Counting the whole level keeps neighbouring boundaries of one level in one layer.

Rejected.

- **Count gaps per individual boundary chain (relative depth).** One sparsely mapped region classifies differently to its neighbours.
- **Maintain a per-country table only.** Countries nobody has looked at would get no locality layer.

Evidence.

Japan puts cities, towns, villages and Tokyo's special wards at level 7, tagging many place=city, town or village, and the wards of designated cities at level 8 with place=suburb, so 7 is locality and 8 lies below.
An Australian extract has every suburb at level 9 and nothing at 8, so 9 becomes the locality layer that address queries match on, and without that an Australian Pack answers no structured query.
A German extract fills 8, 9 and 10, so 8 keeps locality and 9 settles below it.
The ACT has a district at 7 and suburbs at 9, and two spare levels in one country must take two layers.

### An area can hold more than one tier, but only when its tag contradicts its level

Where. `TierSet` in `src/pack/context.rs`; `boundary_tiers` in `src/builder/osm/boundary.rs`.

Each area in a chain stores a set of tiers, one bit per layer, and sorts at its outermost tier.
An admin boundary gains a second tier only when its place tag disagrees with what its admin level means.
A place area never gains one, because it takes its layer from its tag and giving it both would say an island is also a city.

Rejected.

- **One tier per area.** Whichever tier is stored, the other true fact is gone, so a query for a city named Berlin and a query for the states of Germany cannot both be answered.
- **Treat the place tag as a second role whenever it differs from the ranked tier.** The tag is what ranking reads to place a level, so every tagged suburb filed under two tiers.
  It put every ACT suburb in both the neighbourhood and the sub-locality of its address.

Evidence.

Berlin is a German state by admin level and a city by tag.
Tokyo is a city and a prefecture, Singapore a city and a country, Washington DC a city and a top-level region.

### Best boundary per tier is found in one pass

Where. `best_of_each_tier` in `src/builder/osm/boundary.rs`.

One pass over the covering boundaries keeps the best of each tier and flags a tier wanted by two as ambiguous.

Rejected.

- **One filtering pass per tier.** This runs for every record of the build, and a pass per tier allocated a vector for each of the ten tiers even though all but two or three are empty.

### Boundary shapes live in a mapped scratch file in bands, not on the heap

Where. `encode_shape`, `covers` and `Shapes` in `src/builder/osm/containment.rs`.

Each built area is written to one scratch file as a block.
The block holds its rings as OSM 1e-7 fixed-point coordinates and its edges filed into horizontal bands of equal height (about 16 edges per band, and a shape whose long edges would need more than 4 entries per edge halves its bands until they fit).
A point test reads only the band its latitude falls in and casts a ray east across that band's edges.
The file is memory mapped, so shapes take page cache the system can reclaim, not heap.

Rejected.

- **Keep built boundary shapes on the heap.** The boundaries of a planet hold several times the memory a build is given.
- **Test a point against every edge of the shape.** An island drawn from a coastline of a million vertices would cost a million edge tests.
  With bands it costs a few dozen.

Evidence.

Planet boundaries are several times the build memory budget.
A coastline of a million vertices costs a few dozen edge tests.

### Build plan costs are per item, from measured Germany and planet numbers

Where. `plan_build`, `BuildPlan::refine`, `ADDRESS_SPILL_BYTES`, `BASE_UNTRACKED_BYTES`, `BOUNDARY_HEAP_BYTES_PER_REF`, `BOUNDARY_SHAPE_BYTES_PER_REF`, `PACK_BYTES_PER_RECORD` and `derived_budget` in `src/builder/plan.rs`.

A build is planned twice, before any work from the machine and input sizes, then again after each scan when items are counted.
Every cost is per item, not per PBF byte, because a PBF byte of Germany holds four times the node references of a byte of Canada so no byte ratio fits both.
Only the pre-scan estimate reads counts from bytes, at the densest input measured (Germany, 4,861,056,214 B), so it errs towards needing more.
Per-item constants come from measurement.
An address in the sorter is 88 B because New Zealand's spill measured 146 B with a copy of the record beside the key, while the key alone is 87.7 B on a million rows shaped like OpenAddresses and 70.5 B on the test Packs.
Address book 33 B (32.8 measured), placed address 20 B (19.5), house sorter 50 B (49.6).
Pack bytes per record is 102 (Germany 2,666,692,193 B for 26,131,991 records).
Untracked heap is 256 MB base plus workers because Germany held 1.6 GB beyond its tracked peak with 24 workers, which take 1.5 GB of it, and the base doubles the rest.
Boundary heap is 4 B per vertex because the planet held 409 MB of descriptions for 203.6 million vertices (2.0 B each) and twice that leaves room for the line index.
Boundary shape scratch is 30 B per vertex because the planet's was 6.05 GB for 203.6 million vertices (29.7 B each).
Boundary vertices per input byte was never measured and comes from the spill study's upper estimate of 15 GB at 64 B a vertex over 95.4 GB of PBF.
The derived memory budget does not set aside boundary heap, since before the scan it is a guess and the budget's heap ceiling makes tracked buffers spill as it grows.
Before a scan, needs only warn, and only a count stops the build.

Rejected.

- **One cost ratio per PBF input byte.** Item density differs by a factor of four between Germany and Canada, so no ratio fits both.
- **Reserve boundary heap out of the derived budget.** Before the scan the boundary heap is only a guess, and the heap ceiling already makes tracked buffers spill as it grows.
- **Keep a copy of the record beside the address key in the sorter.** New Zealand's spill measured 146 B per address that way against 87.7 B with the key alone.

Evidence.

Germany 4,861,056,214 B PBF, 26,131,991 records, Pack 2,666,692,193 B.
Planet 95.4 GB PBF, 203.6 million boundary vertices, 409 MB heap, 6.05 GB shape file.
Germany reads 3.2 billion node references against the planet's 1.75 billion, so the estimate errs high and still fits.

### Checkpoints save cheap state whole and recompute what is cheaper to derive

Where. `State` in `src/builder/osm/checkpoint.rs`.

A checkpoint saves the cheap in-memory state whole and names the scratch files holding the rest.
It does not save what is cheaper to compute again.
The boundary index, context names and country codes are derived again from the saved boundary set and record ids in a fraction of the time the boundary stage took, and the node store's rank index from its bitmap in one pass.

Rejected.

- **Save the boundary index, context names, country codes and rank index in the checkpoint.** Each is rebuilt from saved data far faster than the stage that produced it, so saving them only adds checkpoint size.

### Choosing a record's town and nesting around it

Where. `BoundaryIndex::lies_inside`, `Candidate::cmp_alike`, `compare_side_by_side` and `around_town` in `src/builder/osm/boundary.rs`.

Two areas that each cover the other's inner point are drawn over the same ground when the smaller is more than half the size of the larger, and lie inside neither.
Containment is decided by whether the outer area covers a point inside the inner one, not by size alone, since size orders areas but does not prove one lies in the next.
Among settlements of the same kind the smaller area wins, or the town point whose reach the record lies furthest inside, because a village drawn inside a city is part of the city.
How well known a settlement is plays no part there.
The best area and best point are found apart, and then the one the source names wins, else the more important, else the area.
Ties break on the source object so one input gives one order.
Areas the tiers put on the wrong side of the town move to a free tier on their own side.
Among areas of the same size, the finer admin level counts as smaller.

Rejected.

- **Rank towns by how well known they are.** A house by one village's point is in that village, whatever the next village is called in other languages.
- **Decide containment by area size alone.** A borough and its county are drawn over the same ground and neither lies inside the other.

Evidence.

A borough and its county are the same-ground case.
The inner point of a municipality lies in one of the villages it is made of, which is still a part of it.
A borough that is a city by its own tags yields to the better-known city point inside it.
A Moscow district inside the city moves below it, and the county around a village moves above it.
A merged municipality stays the town over the village it grew from.
Town points are towns themselves and never inside another (Te Awamutu, Hamilton).

### Country codes are interned in the layer ranking

Where. `Countries` and `Countries::intern` in `src/builder/osm/boundary.rs`.

Codes are stored once and every per-country map keys on a small index.
Codes are lower-cased once per country, because the table is keyed in lower case and codes come off OSM tags in upper.

Rejected.

- **Key the maps on a String code.** It meant cloning a code per boundary per lookup, with hundreds of thousands of boundaries across a handful of countries.

### Country coverage counts only countries whose boundary is whole

Where. `BoundarySet::coverage` in `src/builder/osm/boundary.rs`.

Coverage lists the countries whose own boundary is whole, and the subdivisions of the others.
Subdivisions inside a whole country are dropped.

Rejected.

- **Assume an extract has its country.** A country boundary assembles only from every one of its ways, so an extract of one province has the province but not its country.

### Dropped imported rows find their file by dataset id

Where. `find_repeated_houses` in `src/builder/osm.rs`.

Each dropped or disputed imported row is counted on its import file through a table from dataset id to import position, built once before the rows are read.
Rows are not matched to their file by dataset name.

Rejected.

- **Scan the import names for every dropped row.** Far too many string comparisons on a single thread.

Evidence.

A planet build drops 332 million rows from 3,141 files.
Scanning names per row costs 3.2e11 string comparisons on this one thread.

### External sorter spill rules

Where. `ExternalSorter`, `MIN_RUN_FRACTION` and `Spiller` in `src/extsort/mod.rs`.

When the shared pool is full a sorter spills only once its buffer holds at least a sixteenth of its share, because smaller runs cost more than they free.
The test counts items actually held, not capacity reserved ahead.
A pool worker never hands a spill to the spill thread.
It writes the run on its own thread, since a worker waiting on the spill thread could leave that thread no worker to sort on.
The spill queue holds none, so the producer waits while one buffer is written and at most two buffers, both reserved, exist at once.

Rejected.

- **Count reserved capacity when deciding whether a buffer is worth a run.** One growth step of large items would cut a run after its first item.
- **Always hand the spill to the spill thread.** A pool worker waiting on that thread could leave it no worker to sort on.
- **A queue of waiting buffers.** It would allow more than two buffers in memory at once.

Evidence.

MIN_RUN_FRACTION is 16.
The spill thread uses a channel of size 0, and the in-place write is taken when rayon::current_thread_index() is set.

### Floor on the fair share of a spillable buffer

Where. `MIN_FAIR_SHARE_BYTES` and `Budget::fair_share` in `src/builder/memory.rs`.

While buffers that cannot spill hold the pool (such as the node store while ways are assembled), each spillable buffer still gets at least 16 MiB.
The floor is capped at half a fair share of the whole pool, so all floors together go at most half the limit over it.

Rejected.

- **Let the fair share fall to nothing when the pool is full.** A share of nothing makes a sorter write a run per item.

Evidence.

1.2 million street runs for the Netherlands at a 1 GiB budget.

### Imported address rows repeat, dispute or stand alone by distance

Where. `find_repeated_houses`, `REPEATED_HOUSE_RADIUS_M` and `DISPUTED_HOUSE_RADIUS_M` in `src/builder/osm.rs`.

One record answers for one house.
An imported row within 100 m of a kept address of its house agrees with it and is dropped.
A row beyond that but within 1 km of a kept address it does not contradict is dropped too.
If the kept address came from another source, the two sources disagree, so the kept address keeps its point (OSM's when OSM has the house) and is marked disputed.
If it came from the row's own source nothing disagrees, because one source places one house at several points, and the row only repeats the house.
A row farther still, or stating another postcode, locality or area than every kept address near it, is a house of its own.
The 1 km limit stays below half the 2 km street-group cell, so it never joins what the cell grid keeps apart as two runs of one street, and far below the spacing of the places a street name repeats in.
Where two houses of one street name and number lie closer, the postcode or the locality tells them apart.

Rejected.

- **Treat every row beyond the small repeat radius as a new house.** A large rural parcel's centroid lies a few hundred metres from its house, so the same house would appear twice.
- **A radius larger than 1 km.** It would merge two villages' Hauptstrasse, or a rural road crossing a county line where the numbers start over, and join street runs the cell grid keeps apart.
- **Call a same-source second point a dispute.** Japan's land ministry places a block or lot cut by a road or a river at several points and flags one representative in a column OpenAddresses drops.
  Nothing disagrees, the row only repeats the house.

Evidence.

A row geocoded to the wrong end of a street or to another town lies beyond the radius and is kept as the house it says it is.
Examples are two villages' Hauptstrasse and Japan's lots cut by a road or river.

### Join street ways within the locality, not per way

Where. `join_streets` and `resolve_places` in `src/builder/osm.rs`.

The ways of one street name are joined into one street record per area.
OSM draws a road as a way per stretch between junctions, so without joining one road would be as many answers as it has stretches, told apart only by ids.
Ways join within the locality they lie in when that is a settlement, else its district, so a street in a city's ward or local board is named by the city and joins across the wards.
One name is many streets across a country, and a road crossing a town line is a street in each town.

Rejected.

- **One record per OSM way.** A road drawn as many ways would be many answers.
- **Join all ways of a name across the country.** One name is many streets across a country.
- **Join only within a ward.** A street in a city's ward must be named by the city and join across the wards.

### Judge an import source by its first rows, not by open rings

Where. `import_addresses` in `src/builder/osm.rs`; `SAMPLE_ROWS` and `first_points` in `src/builder/addresses/mod.rs`.

The first 1,000 rows of a file are checked against its stated area, and the source is broken only when more than half of them lie outside.
Half outside is not most, and a sample with no point judges nothing.
Rows past the sample are judged one by one.
A region with an open ring (Russia's mainland in the planet) keeps only the rings that closed, and a source is not judged by them, so its rows on the open part are kept.
Rows outside every extract's stated bounds are left out, and an extract that states none keeps every row.
For a file with a declared area, the first 1000 rows are read before any is taken.
If more than half lie outside the area the source is broken and none of its rows is imported, reported as source_outside_area.
The first rows tell, since a source is broken throughout or not at all.

Rejected.

- **Judge a source by an area whose ring did not close.** Rows on the open part would be dropped as outside the area although the source is sound.
- **Drop only the rows that fall outside the area.** The few rows an error lands inside the area are as wrong as the rest, for example with longitude and latitude swapped.

Evidence.

Russia's mainland in the planet has an open ring.

### Names from merged place points go on the boundary's own record only

Where. `BoundaryBuilder::merge_place_point` and `BoundarySet::place_records` in `src/builder/osm/boundary.rs`.

Names gained from place points merged into a boundary are added to that boundary's own record as alternate names.

Rejected.

- **Add them as area names of every record inside the boundary.** It would bloat the text index.

### Narrowest box around parts leaves out the widest empty stretch of longitude

Where. `narrowest_box` in `src/util/geo.rs`; `narrowest_extent` in `src/builder/osm/containment.rs`.

The box around several rings or lines is the narrowest one.
The widest stretch of longitude that no part reaches is left out, so parts on both sides of the antimeridian make a box that crosses it.
Each ring's own box never crosses the antimeridian.
OSM draws an area over the antimeridian as one ring each side meeting at 180 and -180, and those two rings are one area.
With two gaps of one width the box stays in one piece.

Rejected.

- **Plain west-most to east-most box over all parts.** Islands either side of the antimeridian that do not touch it would get a box spanning almost the whole world, though they are far apart the long way and near the short way.

Evidence.

Tests cover a ring ending at 180 with one starting at -180 (one area), islands either side of the antimeridian, and two equal gaps.

### Nine admin tiers, with one tier reserved for each level still to come

Where. `assign_layers_by_containment` and `ADMIN_LAYER_ORDER` in `src/builder/osm/boundary.rs`.

Admin boundaries rank into nine layers, a sub-tier under each conventional tier, so no level competes for a slot and none is dropped.
Each country's levels get a tier outermost first and never go back up.
While placing a level the code keeps one tier for every level still to come, so an outer level's preference cannot eat the ladder and strand the rest at the innermost tier.
A level that wants a tier already passed goes to the sub-tier instead.
A level wanted by several deeper levels yields a tier so the innermost keeps the shared one.
A country deeper than the nine tiers ends with its innermost levels sharing the last tier, and the chain keeps the outermost of the pair rather than dropping it.

Rejected.

- **Five tiers, with levels competing for them.** The levels that lost the race were dropped from every address of the country.
- **Let outer levels take the tier they prefer without reserving room.** An outer preference strands deeper levels at the innermost tier.

Evidence.

With five tiers, four of Germany's nine levels (three state_district, two city_district) were dropped from every German address.
Taiwan calls its level 4 a city, which alone would spend six tiers before its five deeper levels are placed.
Australia calls both its 9 and 10 a suburb, so 10 is the neighbourhood and 9 the sub-locality above it, where before one of the two was dropped.
A Canada Pack with every level 2 to 11 has ten levels for nine tiers.

### One sorter is capped in size because huge buffers are slower

Where. `BuildPlan::sorter_bytes` and `MAX_DERIVED_BUDGET_BYTES` in `src/builder/plan.rs`; `MAX_DERIVED_BUDGET_BYTES` and `derived_budget` in `src/builder/memory.rs`.

The most one sorter may hold is capped at the derived budget ceiling, and the rest of the budget serves the node store.
Sorting and indexing in buffers far larger than the CPU caches cost more than the spill they save.
Germany with a 6.2 GiB budget scanned, joined and wrote records slower than with 1 GiB.
The derived memory budget is clamped between MIN_BUDGET_BYTES and 1 GiB, and takes half of the usable memory below that.
More memory than 1 GiB is not used even on a big machine, because sorting, hashing and indexing in buffers far larger than the CPU caches cost more than the spill they save.
A small machine gets less, so the build spills sooner instead of paging.

Rejected.

- **Give each sorter the whole memory budget.** Germany at 6.2 GiB ran slower than at 1 GiB.
- **Let the budget grow with the machine's memory.** Past 1 GiB a build gets slower, not faster.
  Buffers far larger than the CPU caches cost more than the spill they save.

Evidence.

Germany, 6.2 GiB against 1 GiB.
Germany with a 6.2 GiB budget took 57, 63 and 278 s to scan, join nodes and write records, against 49, 46 and 194 s with 1 GiB, on either allocator.
It spilled 9.8 GB instead of 15.0 GB to an SSD that was never the bottleneck.

### OpenAddresses job cache and download retries

Where. `cached_job`, `store_job` and `RETRY_WAITS` in `src/builder/addresses/openaddresses.rs`.

A job record is written beside its final name and kept only once it reads as a job, so a cut-off or garbled reply is never cached.
An unreadable cached record is removed so the next build asks again instead of failing on it.
A download that fails on the way (dropped connection, busy server) is retried after waits, because one stalled connect should not cost the rest of the build.

Rejected.

- **Fail the build on the first failed download or unreadable cached record.** A large build would lose everything fetched so far.

Evidence.

A Canada build fetches some 200 sources.

### Postcode centre is a medoid of a hash-ordered sample, keyed by country

Where. `Group::medoid`, `Accumulator` and `SAMPLE_POINTS` in `src/builder/osm/postcode.rs`.

A postcode point is the sampled member address with the least total distance to the others (a medoid), not a mean.
A real address is always returned, a few stray addresses do not move it, and it stays correct across the antimeridian where a mean of longitudes does not.
The sample keeps the 16 member points with the lowest hashes, so the result is the same whatever order addresses arrive in and memory is bounded by the number of postcodes, not addresses.
A medoid costs 256 great-circle distances per postcode and a country has hundreds of thousands of postcodes, so they are computed in parallel.
Postcodes are keyed by country, never by input file, because a planet file holds AU and NZ "2110" apart and a country split over two extracts makes one of each code.
Repeated identical points count once, so a block of flats does not outvote a street.

Rejected.

- **Mean of member coordinates.** A mean of longitudes breaks across the antimeridian and is dragged by stray addresses, for example one address mistagged with a Toronto postcode among Sydney ones.
- **Keying postcodes by input file.** A planet file holds AU and NZ "2110" as separate postcodes, and a country split over two extracts would yield one point per extract.

Evidence.

AU and NZ both use "2110".
256 distances per postcode, hundreds of thousands of postcodes per country.

### Records that stay in memory are rounded as a spill would round them

Where. `as_spilled` and `encode_geometry` in `src/builder/osm/spill.rs`.

A geometry a sorter keeps in memory is passed through the spill encoding and decoding so it holds exactly what a spilled one reads back.
Line vertices spill as 1e-7 degree deltas and are read back by a division that the node store's conversion does not always match to the last bit.
Without this, a record kept in memory would differ from a spilled one, so a street joined from it would get another representative point and, near a border, another context.
The delta encoding keeps the record store's precision without its range limit, so an out-of-range vertex fails the build the same way whether or not it was spilled, and non-finite vertices keep their raw bits for the same reason.

Rejected.

- **Keep in-memory geometry as converted from the node store.** It can differ from the spilled copy in the last bit, which changes a joined street's representative point and a border record's context.

### Street ways of one name join within 50 metres

Where. `JOIN_GAP_M` and `is_settlement` in `src/builder/osm/street.rs`.

Ways of one name in one area whose ends lie within 50 m are one street.
Ways of a road split at junctions share a node there, and a roundabout or a bridge named otherwise leaves a gap of tens of metres, while two roads of one name in one locality lie farther apart.
A place counts as a settlement only by its OSM place= value, so an admin area that is none, such as a ward or a local board, is not where its streets are said to be.

Rejected.

- **Join only ways that share a node.** Roundabouts and bridges named differently leave gaps of tens of metres and would split one street.

Evidence.

0.0003 degrees of longitude is about 30 m, inside the gap.

### The locality anchor and folded levels

Where. `anchor_level` and `folded_levels` in `src/builder/osm/boundary.rs`.

Each country has an anchor level for the locality tier, the tier an address query matches a town on.
It is the outermost level the table calls a city, else the outermost below the district.
When more levels lie above the anchor than tiers above locality, the outermost levels that claim no tier fold into the tier inside them and give up a tier of their own.
Where two share a tier the smaller boundary wins it.
The anchor is also given room to reach the locality tier, so an outer level that settles too deep cannot push it past its own.

Rejected.

- **Give every unnamed outer level its own tier.** The arrondissements took the locality tier and the communes the one below.

Evidence.

Australia's level 9 is tagged a suburb and is still the town an address names.
France in the planet has levels 3 (France metropolitaine) and 5 (Collectivite europeenne d'Alsace) that no table row names, beside 2, 4, 6 and 7 above its communes.
Nouvelle-Aquitaine beats France metropolitaine for the shared tier.
A smoke test over the whole table checks that every country with a town fills locality, including sub-national extracts that have no level 2 boundary and know the country only from the ISO3166-2 tag.

### The way pass classifies ways again instead of keeping them

Where. `way_features` in `src/builder/osm/scan.rs`.

The way pass classifies the ways a second time with way_features.
A reread of the way blocks costs less than writing every kept way's tags to scratch and reading them back.

Rejected.

- **Keep each classified way between the passes by writing its tags to scratch.** Writing every kept way's tags to scratch and back costs more than rereading the way blocks.

### Town reach is half way to the next town of its kind or bigger

Where. `measure_reach` in `src/builder/osm/towns.rs`.

A town reaches as far as the farthest address that names it and lies nearer to it than to any other town point of its kind or a bigger one.
An address past the half way line is that other town's, or names a town of this name by mistake.
Every town reaches at least half way to the nearest other town point, so a town no address names still holds the houses nearest it.

Evidence.

The code builds one tree per rank of towns of that kind and every bigger kind.

### Which OpenAddresses sources a country or region takes

Where. `select`, `dataset_of` and `licensed` in `src/builder/addresses/openaddresses.rs`.

A country covered whole takes every source under its path, countrywide and the others alike, because they come from different publishers and need not cover the same addresses, and the build drops a row repeating an address another source has.
A region covered in part takes only the sources under its path (CA-ON takes the ones under ca/on/).
A data set listed twice is taken once, at its latest job.
Dataset names are computed from every needed source before licence filtering, so leaving a source out for its licence never renames another's records.

Rejected.

- **Take the countrywide source for a region.** It would bring in the rest of the country.
- **Take only the countrywide source for a whole country.** Other sources come from different publishers and cover addresses it lacks.

Evidence.

CA-ON maps to sources under ca/on/.
Datasets look like openaddresses:nz/countrywide.

## Reading OSM data

### Admin levels come from a per-country table, not code

Where. `verdict` and `AdminLevels` in `src/builder/osm/admin_levels.rs`.

Which layer each admin level is comes from a vendored table read the same way for every country, with rules by OSM id (London is a city), by level inside another boundary (a level 10 inside Berlin is a neighbourhood), by designation (a British unitary authority is a district) and by level (Germany's 5, 6 and 7 are districts, 8 cities).
A country or level the table does not list keeps the layer the boundary code gives it.
A group of regions (関東地方) counts as a region because queries name it, and a record keeps the smaller region (its prefecture) as its own.
A country described only down to its regions (the UAE and its emirates) has no locality tier, and no ranking can invent one.

Rejected.

- **Invent a locality tier by ranking when the table has none.** No ranking can invent a tier the data does not describe.

Evidence.

Germany's levels 5, 6, 7 and 8, London, Berlin level 10, the UAE emirates and 関東地方.

### Admin tiers are deeper than the conventional five and search as their parent tier

Where. `PlaceLayer` and `Layer::from_place` in `src/record.rs`.

The place tiers include SubRegion, SubDistrict and SubLocality between the conventional ones.
Germany maps nine admin levels, three of them state_district and two city_district.
With one tier each those collapse and all but one boundary of each is dropped from every German address.
A Sub tier is stored and searched as the tier above it, so a query for a district still matches a subdistrict, while the exact tier is kept on the context entry that the response reports.
Place sorts last because it is no admin tier, since an island can be smaller than a locality or larger than a region.
Tier discriminants are the on-disk encoding and cannot be renumbered without a version bump.

Rejected.

- **One tier per conventional level only.** German addresses would lose all but one boundary of each of the repeated levels.

Evidence.

Germany has nine admin levels, three state_district and two city_district.

### Closed-for-good objects are not POIs, and POIs also emit address records

Where. `emit_poi` and `is_poi` in `src/builder/osm/poi.rs`.

A lifecycle prefix such as disused, abandoned, was, demolished or razed makes a different key that no preset reads.
Something closed for good is therefore no place, and no record can claim it is closed.
A POI that states a valid address also emits an address record, because address search reads addresses alone and would otherwise miss every house only a POI states.

Rejected.

- **Keep lifecycle-prefixed objects as POIs flagged closed.** No preset reads those keys, so the record could never say it is closed.
- **Leave a POI's address only on the POI.** Address search reads addresses alone and would miss every house that only a POI states.

Evidence.

The prefixes tested are disused, abandoned, was, demolished and razed, with a pub named De Pindol as the example.

### Node store keyed by rank in a bitmap of needed ids

Where. `NodeSet` and `Locations` in `src/builder/osm/locations.rs`.

The scan marks every node a kept way or boundary member references in a bitmap over the id space.
The node pass keeps only the coordinates of marked nodes, appended in id order, and a node's slot is its rank (the number of marked ids below it), answered by a popcount index in a few word reads.
Marked ids the input lacks are unmarked as the pass goes so ranks stay dense.
This costs 8 B per needed node plus 0.13 B per id of the id space, and og needs about one node in seven.
Coordinates are memory-mapped from scratch when the budget cannot hold them.

Rejected.

- **Sort references and coordinates by way id through scratch.** It wrote 21 B per reference to scratch twice over.
- **Store every node, as dense and sparse index stores in other OSM tools do.** Costs 16 B per stored node and stores nodes no way needs.

Evidence.

8 B per needed node plus 0.13 B per id, against 21 B per reference written twice by the sort approach.
About one node in seven is needed.

### POI duplicates merge on exact keys, never on distance

Where. `resolve`, `one_place` and `conflict` in `src/builder/osm/poi.rs`.

Entity resolution for POIs is blocked by name and uses exact keys only.
Two records are one place when nothing conflicts and one rule holds.
The rules are the same Wikidata item, or the same kind with one point inside the other's outline, or the same kind and address, or the same kind drawn with a shared node (a canal in pieces).
A shared Wikidata item alone is not enough.
Records that state different addresses, items, websites, phones or refs are two places whatever else they share.
Each record joins the leader, never another member, so a run cannot chain.
Merged ids stay as aliases.
A wrong merge hides a real place while a missed one only shows it twice, so the rules lean toward not merging.

Rejected.

- **Merge by same name plus distance.** Same name and distance mostly join different things, such as chalets 9B and 9D, two pubs facing each other, or both sides of a bus stop.
- **A shared Wikidata item alone as proof of one place.** An organisation tags every one of its offices with its own item.
- **Center clustering where a record can join any member.** A run of records could chain into one place.
  Joining only the leader prevents that.

Evidence.

Examples are chalets 9B and 9D, two pubs facing each other, both sides of a bus stop, a canal drawn in pieces, and an organisation whose every office carries one item.
A chain's name group holds thousands of copies, so outlines are held in an R-tree to keep each test to the outlines that could cover the point.

## Pack format

### Admin context is a variable-length chain, not a fixed tuple of slots

Where. `AdminContextTuple` and `MAX_CHAIN` in `src/pack/context.rs`.

A record's context is the chain of areas it lies in, outermost first, with a length that follows the country's hierarchy.
The build keeps every boundary, and the lossy step moves to the response, where it is recomputed per request and costs nothing permanent.
Chains are interned and a record stores only the chain id.
A chain is bounded at 32 entries so a corrupt length cannot drive a huge allocation.
Chains vary in length, so offsets are walked once at open.

Rejected.

- **A fixed tuple of named slots (the first format held six slots).** It has to choose which of a country's levels to keep, and the dropped ones are gone from every address in that country for the life of the Pack.

Evidence.

Germany maps nine admin levels and Monaco two.

### Country and place areas contribute only their main name to record text documents

Where. `indexed_area_names` in `src/pack/mod.rs`.

A record's text document carries every name of each area it lies in, except the country and the place area (an island), which are indexed by their primary name alone.
The parser's dictionaries still hold every name.

Rejected.

- **Index every language name of the country and place on every record.** A country or an island has hundreds of names, one per language, and every record in it would carry them all.

Evidence.

Hundreds of names per country or island, one per language.

### Publishing keeps old generations and sets keep before CURRENT can name the new one

Where. `publish` in `src/pack/publication.rs`.

Old Pack generations are kept when CURRENT moves because running servers may still map them.
The pointer is written under a unique temporary name so simultaneous builders never share writes.
The keep flag is set the moment CURRENT may name the generation, so a later failure never deletes the Pack that servers are told to open.
Before the switch the Pack is opened with the serving readers, verified, and its text documents are checked to cover every record once.

Rejected.

- **Delete previous generations on publish.** Running servers may still have them mapped.
- **A fixed shared pointer file name.** Simultaneous builders would share writes.

### Record store interns strings per segment and writes address runs as continuations

Where. `SEGMENT_RECORDS`, `DICTIONARY_BUDGET_FRACTION` and `AddressRun` in `src/pack/record_store.rs`.

Strings are interned per segment, not in one dictionary of every distinct string of the input, so the build holds only one segment's dictionary in memory.
A segment ends after SEGMENT_RECORDS records or once its dictionary uses its share of the build budget (one eighth), whichever comes first, and only between blocks, so a continued address reads its anchor's strings from the same segment.
Records are in Hilbert order, so a segment covers one area and most of its strings are its own.
Only widely shared ones (countries, regions) repeat across segments.
The string table uses FxHash, not SipHash, because every string of every record is looked up on the one thread that writes records in order, and the strings come from the build's own input file.
An address that states the same street, place, areas, locality, region, postcode, country, street postcodes, building and admin context as the one before it in its block is written as a continuation holding only number, unit, point and id as small deltas.
The run's first address in the block, its anchor, holds every shared field, so any record decodes from its own block.

Rejected.

- **One global string dictionary for the whole build.** It would keep every distinct string of the input in memory.
- **SipHash for the string table.** Input is the build's own file, so hash-flooding resistance buys nothing on a hot single-threaded lookup.

### Record writing runs three jobs on their own threads, and the text merge starts early

Where. `Writer::commit`, `Merging` and `Sealing` in `src/pack/mod.rs`.

When a batch is committed, the dictionaries, the text index and the record store with its spatial pairs each run on their own thread, joined per batch, so each still takes records in order.
Merging the text index's segments is most of sealing and runs on one thread, so it starts at once beside the parts sealed before it.

Rejected.

- **Feed the three consumers from one thread in turn.** They read none of each other's state, and one thread feeding them in turn bound the whole step.

### Records are stored in Hilbert order, not Morton order

Where. `hilbert_key` in `src/util/hilbert.rs`.

Records are ordered along a Hilbert curve so neighbours on the map are neighbours in the Pack.
A reverse lookup or a batch of nearby addresses then touches a few adjacent pages, and delta-encoded coordinates stay small.
Latitude is stretched by 2 before indexing so the grid is square, since longitude spans 3.6e9 units and latitude 1.8e9.

Rejected.

- **Morton order.** It has long jumps at quadrant boundaries, which Hilbert does not.

Evidence.

Longitude spans 3.6e9 units and latitude 1.8e9 before stretching.

### Scratch ledger degrades by compressing harder, and Windows scratch files are marked temporary only while they fit the cache

Where. `Scratch` and `raise` in `src/extsort/scratch.rs`.

A refused reservation steps zstd through levels 1, 3, 6 and 9.
Level 1 compresses at several hundred MB/s per core, and past 9 the gain is small and the cost steep.
Only after that does the build stop.
Scratch files are synced only once a checkpoint names them.
On Windows they are marked temporary only while what was spilled fits the page cache the build leaves free, because the lazy writer then keeps them in memory, but past that it would push the Pack and the input out of the cache.
Retired files give way first, and the checkpoint they belong to is given up, never the build.
Peak counters read first before fetch_max, because a written block rarely moves a peak and fetch_max takes the cache line exclusive even when it does not.

Rejected.

- **Mark every scratch file temporary on Windows.** Past the free cache size it would push the Pack and the input out of the cache.
- **Compress above level 9.** The gain is small and the cost steep.
- **Call fetch_max on every block.** It takes the cache line exclusive even when the peak does not move.

Evidence.

Level 1 compresses at several hundred MB/s per core.

### Spatial index gives lines and context points their own trees

Where. `LINES`, `BLOCKS` and `CONTEXT` in `src/pack/spatial_index.rs`.

There are three packed R-trees. spatial/blocks has one box per record block around its point records, so a query decodes the blocks near it whole, which takes microseconds for 64 records, and a box per block costs a fraction of a byte per record. spatial/lines has one box per street or interpolation line, because one long road would stretch a block's box over every query along it. spatial/context has one box per place and postcode point, so wide-radius admin lookups pass over the address blocks.

Rejected.

- **Put lines inside the block boxes.** One long road stretches a block's box over every query along it.

Evidence.

Decoding a 64-record block takes microseconds.

### Stable ids cover only OSM objects, via a perfect hash that stores no id

Where. `key`, `build` and `find` in `src/pack/ids.rs`.

The id lookup holds only OSM places, areas and streets.
Imported OpenAddresses rows are left out (key returns None for a Row), because their row ids renumber every release, so a saved one would open another house.
A street answers to any of its ways' ids, since a map tile names the way a tap lands on.
The lookup stores no ids.
A minimal perfect hash (FMPHGO from the ph crate, about 2 to 3 bits an id) maps each id to a slot holding its first block number.
Because the hash also sends unknown ids to some slot, a lookup checks the record's own id.
An object that is records in several blocks (a way that is both a POI and a district) has its other blocks listed apart, by key.

Rejected.

- **Stable ids for imported OpenAddresses rows.** Row ids renumber every release, so a saved id would open another house.

### Text index layout choices for ranking and compression

Where. `CHUNK_DOCUMENTS`, `Fields::place_name_text`, `unique_words`, `Document::from_record`, `PostcodeAreas` and `TextIndexWriter::add` in `src/pack/text_index.rs`.

Documents arrive in Hilbert order and each part takes 8,192 in a row before the next part takes the next ones, so every posting list runs in long stretches of the map, which pack into far fewer bits than the one-by-one spread of a many-thread writer.
Place-name text is its own field apart from address fields so a word common in shop names stays rare among street names, while context stays shared because one list of a town's neighbours compresses far better than two.
Each word of a record counts once, since without field norms a repeat such as "Walmer Road Parkette, 227 Walmer Road" or "Upper Queen Street" also named "Belgium Street" would add to its score and a record would outrank the one a query names on repetition alone.
What a record is and where it is go to separate fields so a name word is not matched by an area's name ("Kingston Road" by an address in Kingston, "Danforth Avenue" by one in the Danforth neighbourhood).
The where field holds locality, region, country, postcode and context names because a POI name alone rarely identifies one place ("Tim Hortons") and most addresses carry no addr:city.
Postcode centroids rank a record that states no postcode by its nearest one, since most addresses state none and a query's postcode could otherwise rank only the few that do.
Documents are added one at a time so Tantivy's bounded indexing queue blocks the producer when the indexer falls behind, where a batch would be one message and the queue would hold thousands of batches.

Rejected.

- **One shared field for all names and address text.** A word common in shop names would be common among street names too.
- **Feeding documents to the indexer in batches.** A batch is one queue message, so the queue would hold thousands of batches and every record of the build.
- **Count repeated words with field norms off.** Repeats would add to the score so repetition alone could outrank the named record.
- **Rank postcode only from stated postcodes.** Most addresses state none.

## HTTP API

### /places is a batch item and not a /jobs endpoint

Where. `PLACES` in `src/http/server.rs`.

/places is offered as a batch item in QUERY /places.
A job is a file of addresses or points too long for one request, and one batch already holds a saved-place list, ids and names alike.

Rejected.

- **Add /places as a /jobs endpoint.** Jobs are for files too long for one request, and a batch already covers a saved-place list.

### A kind-of-place query means the nearest ones but a nearby brand still answers

Where. `nearest_of_category` in `src/http/server.rs`.

A query that only names a kind of place, such as pharmacy or apotheek, returns the nearest of that kind whatever their names.
A place near the caller named as typed, such as Subway, still answers, so a brand that is also a category word keeps its meaning.
What else matched fills the rest.
In the town lookup order, a nearby town comes first, then one known abroad such as new york, then a nearby POI, since pike place is the market here and not a street in Auckland.

Rejected.

- **Treat a category word only as a category.** A brand that is also a category word would lose its meaning.

Evidence.

Subway, pharmacy, apotheek, pike place versus a street in Auckland.

### A list of ids is bounded by reading one past the limit

Where. `place_ids` in `src/http/server.rs`.

The ids list takes at most one past the limit, which is enough to refuse it, so a list of a million ids costs no more than one just over the limit.
The limit is the server's batch item limit, so one setting caps lookups whether they come as items or as ids.

Evidence.

A million ids.

### Answers name a place one way, and highlights follow renames

Where. `name_stated_parts`, `move_highlights` and `relabel` in `src/response.rs`.

Each place the source states (town, region, country or a named part of a town) is written as the area of the result's context that it names, so one answer never names a place two ways.
An address tagged Vienna in Wien says Wien, and with lang says what the area does.
Codes such as CA or AT stay as written because a code is the same in every language.
A part that names no area stays as the source wrote it.
A highlight inside a renamed part is dropped, because the typed text matched the local name and not the new one.

Rejected.

- **Leave the source's own spelling of the town in the address.** The answer would name one place two ways, Vienna in the address and Wien in the context.

Evidence.

Vienna tagged in Wien.

### Basemap is served raw with no guard and shutdown stops jobs first

Where. `build_router` in `src/http/server.rs`.

The basemap PMTiles is served raw with native range support and no full-file guard, as a local-dev convenience.
Abuse protection belongs to the deployment layer, where it is served from R2 with free egress.
HEAD is refused on single-method routes and not routed to a GET handler.
On shutdown jobs are cancelled first, because a job's upload is a request as long as the job, so requests drain only once jobs stop.

Rejected.

- **Guard full-file fetches of the basemap in the runtime.** That is a deployment-layer concern.

### Batch items go through the same query string as GET

Where. `item_query`, `RawItem` and `Lookup` in `src/http/batch.rs`.

Each batch item is turned into the query string a GET would have been sent (shared parameters overridden by the item's own) and parsed by the same code, so an item means what the same GET means with one parser, one set of rules and one set of errors.
A field named twice fails the item like a GET with a repeated parameter does.
A bad item never fails the batch.
The prepare step sees the server so operator limits and Pack-dependent rules refuse an item as the GET would.

Rejected.

- **Read the item as JSON straight into the typed parameters.** It would give a second parser with different rules and errors than GET.
- **Keep the last value of a repeated field.** GET with a repeated parameter fails to parse, so the item fails the same way.

### Batch slots equal usable CPUs and batch work runs at low priority

Where. `open` in `src/http/server.rs`.

Batch slots equal the usable CPU count, because batches and jobs share the pool's cores and running more at once than there are cores only slows each one down.
The batch pool uses every CPU at low priority so batches take what single lookups leave idle and a single lookup never waits for a core.

Rejected.

- **Allow more concurrent batches than cores.** It only slows each one down.

### Job directory is per port and lookups run several per CPU

Where. `default_dir` in `src/http/jobs.rs`; `LOOKUPS_PER_CPU` in `src/http/limits.rs`.

Each server keeps its jobs in its own directory under the system temp directory, one per port, because a server starting up fails the jobs it finds running and two servers sharing one directory would fail each other's.
Single lookups default to 4 per CPU, because a lookup on a Pack larger than memory waits on page faults and the core is free meanwhile.

Rejected.

- **One shared jobs directory.** Two servers would fail each other's running jobs on startup.
- **One lookup per CPU.** Cores idle while a lookup waits on page faults.

### Lookup slot is held by the work, and a timeout has no Retry-After

Where. `lookup` in `src/http/server.rs`; `timed_out` in `src/http/problem.rs`.

The work holds the lookup slot, not the future, so a lookup whose client left holds it until it stops at its next check.
A lookup past the timeout is answered 503 timeout with no Retry-After because the same lookup takes as long the next time.
A refused lookup says retry after 1 second because lookups take milliseconds.
Busy is refused at once and not queued, so a client learns to come back and does not hold a connection open.

Rejected.

- **Queue lookups when all slots are taken.** A client would hold a connection open and not learn to come back.
- **Send Retry-After on a timeout.** Retrying would take as long again.

### Missing, empty and valued parameters are told apart once

Where. `Param` in `src/http/param.rs`.

A parameter is Missing, Empty (only whitespace) or Value.
A form that always sends every key sends "locality=" for a blank field, so empty and missing are different answers to whether the caller gave the value.
They are separated once in Param and every handler matches all three cases.

Rejected.

- **Treat empty as missing.** Forms send blank fields as empty strings, which is not the same as the caller leaving them out.

### OpenAPI spec is generated and then adjusted to match what the server really sends

Where. `transform`, `drop_nulls`, `add_boundary_responses` and `bounds_match_the_constants` in `src/http/openapi.rs`.

The spec is generated from the Rust types, not written by hand, so a field cannot change shape without the spec changing, and contract_tests fails when contract/openapi.json and the code disagree.
After generation the null branch of every Option is dropped, because the API never emits a null and an absent value is an absent key.
Both shapes are handled, the type union and oneOf with a null.
A oneOf of one is inlined.
Every route's 405 is added by hand because it comes from router fallbacks, not a handler.
HEAD is never taken (the router answers 405 instead of routing it to GET).
404 is deliberately not hung on each operation, since it belongs to paths not in the document.
OpenAPI 3.1 has no query operation, so a batch is documented on its post.
Parameter bounds are literals because the attribute macro cannot read a constant, and bounds_match_the_constants stops drift.
Problem is described by a separate type from the one that renders it, so a test holds their field names together.

Rejected.

- **Hand-written spec.** It could drift from the types that serve the routes.
- **Leave generated nulls in the spec.** It would tell a client to expect a value the API never sends.
- **Attach 404 to every operation.** It belongs to paths that are not in the document, so it would say the wrong thing.

### Problem Details for 404 and 405

Where. `not_found` and `method_not_allowed` in `src/http/method.rs`.

Unknown paths return 404 and disallowed methods return 405, both as application/problem+json with the request id added by middleware.

Rejected.

- **An empty body.** Clients get no machine-readable problem.
- **A static-file 404.** It is not the Problem Details shape the rest of the API returns.

### Problem type stays about:blank and instance is omitted

Where. `DOCS_BASE` and `ProblemInner` in `src/http/problem.rs`.

DOCS_BASE is None, so type renders as about:blank, RFC 9457's default and the only honest value while nothing can be dereferenced.
A type that looks like a link but 404s is worse than one that never promised anything.
Setting a base later is additive but changes every error body, so it lands with the docs pages. instance is left out because request_id already identifies the occurrence and the logs are indexed by it. error_code is the stable extension value to branch on, since title and detail are prose that may be reworded.

Rejected.

- **Emit a documentation link in type before the pages exist.** It would 404.
- **Add an instance member or a separate link member.** request_id already does the job of instance, and type already is the link.

### Public origin never reads forwarded headers, and logs never carry the query

Where. `origin` in `src/http/server.rs`; `internal_error` in `src/http/request_id.rs`.

Absolute URLs use the operator's public URL, else the Host the client connected with.
A forwarded header is never read because any client can send one.
The 500 log line names the request, route and cause and not the query, which is personal data.
Path parameters can carry addresses too, so the route is logged by pattern.

Rejected.

- **Trust X-Forwarded headers for the origin.** Any client can send one.

### Query length is counted in characters after whitespace collapse

Where. `validate_query_length`, `BATCH_ITEM_BYTES` and `parse_bbox` in `src/http/bounds.rs`.

A query is trimmed and every run of whitespace becomes one space before its length is counted, in Unicode scalar values and not bytes.
An address pasted from a web page can carry hundreds of spaces and line breaks of its layout between a few words and is still a query for them.
Batch bodies read 4096 bytes per item, because an item with every field at full length is about 1.2 KiB of ASCII, which leaves room for escapes and non-Latin text.
A bbox across the antimeridian is two requests, as its west edge would lie east of its east edge.
A blank focus or bbox is treated as absent, as a map that always sends the key sends it blank when the caller has no point or box.

Rejected.

- **Count the raw length in bytes.** Layout whitespace in pasted addresses would reject real queries, and multibyte text would hit the limit at a third of its visible length.

Evidence.

A full item is about 1.2 KiB of ASCII, and the item budget is 4096 bytes.
A test checks 256 copies of the two-byte letter e-acute are accepted and one more is rejected.

### Startup warms the Pack and category tables before serving

Where. `open` in `src/http/server.rs`.

The Pack is read in at startup when it fits, so the first queries do not each wait on faults.
Tables built once per process, such as categories, are loaded up front.
They take up to a second on a slow build and would otherwise eat the first lookup's deadline.

Rejected.

- **Build the tables on first use.** They take up to a second on a slow build and eat the first lookup's deadline.

Evidence.

Up to a second on a slow build.

### Unknown layer or malformed country code is a 400, an uncovered country is an empty answer

Where. `parse_layer`, `parse_countries` and `parse_lang` in `src/http/server.rs`.

An unknown layer, a country code that is not two letters, or a malformed language is a 400 naming the problem, because a typo that silently matched nothing would look exactly like no results here.
A valid code for a country the Pack does not cover is not rejected, since that is a legitimate empty answer.
An empty value means no filter, as a form that always sends the key produces.
A blank locality, region or postcode narrows nothing.
An empty q next to an address is still a conflict, because which one the caller meant is a guess.
Layer is kept as a string so the error lists the valid layers instead of a serde unknown variant message.
A country filter that cannot be a code (country=Canada) is rejected like an unknown layer, as a typo.
A well-formed country the Pack does not cover (country=FR) is not an error and gives an empty answer.

Rejected.

- **Treat an unknown layer or bad country as a filter that matches nothing.** A typo would look exactly like no results and a client could not tell them apart.
- **Silently match nothing for a malformed country.** A typo would look like a real empty result.

Evidence.

Contract tests use /geocode?q=king&country=Canada and country=FR.

## Vendor-compatible APIs

### A cuisine matches a food type only by its whole name

Where. `named`, `NAMED` and `of_cuisine` in `src/compat/here/food_types.rs`.

A cuisine value is the food type of the same whole name, read with underscores as spaces and ignoring case and a plural s (ice_cream is Ice Cream, burger is Burgers).
Common values HERE files as a variation of one country's kitchen or spells otherwise sit in a small hand-reviewed table (sushi, ramen, tapas, kosher, cantonese, szechuan, sichuan, crepe, pastry).
A part of a variation's name names nothing, so chicken is Chicken and not Japanese-Yakitori/Chicken.
Fish, bakery and northwestern name no kitchen of one country.

Rejected.

- **Match a cuisine to a food type by any word of the food type's name.** Chicken would become a Japanese Yakitori/Chicken variation, which is a kitchen of one country.

Evidence.

chicken versus Japanese-Yakitori/Chicken.
Tests also cover fish, bakery and northwestern.

### Accept-Language that names no language is ignored

Where. `accept_language` in `src/compat/google/language.rs`.

Autocomplete reads the first tag of the Accept-Language header (de-CH of de-CH,de;q=0.9) when the body names no language.
A missing header, the wildcard, or a tag that is not a language code gives no language and the request still succeeds.
The language og names a record in is the language asked as og holds it (de when de-CH was asked and og has only a de name), else the language of the local name.

Rejected.

- **Refuse a request whose header names no language.** A browser's header is no request to refuse.

Evidence.

Test inputs are de-CH,de;q=0.9, *, and not a language.

### Each OpenAddresses data set is judged by the licence its job states now

Where. `judge` and `pinned` in `src/licences.rs`.

A build judges a data set by the licence OpenAddresses states on the job whose output it reads, not by the pinned snapshot, so terms a publisher changed since the pin decide.
Rules read a standard licence's URL and a few phrases of free text, restriction first, so CC BY-NC-ND is restricted.
A hand-read decision in the catalog overrules the rules, and only while the stated text is exactly the text that was read.
The pinned snapshot is used only for a job that states no licence.

Rejected.

- **Judge by the pinned snapshot's licence.** A publisher may have changed its terms since the pin.

Evidence.

CC BY-NC-ND is restricted.

### Google-compatible Places refuses what og cannot honour and answers every field without a mask

Where. `field_mask` and `Viewport::bbox` in `src/compat/google/mod.rs`; `restriction`, `photo_media` and `keeps_only_unheld` in `src/compat/google/places.rs`.

Google refuses a Places request without a field mask (X-Goog-FieldMask, else $fields, else fields). og answers every field instead.
A viewport that crosses the antimeridian is refused, because og cannot search such a box and dropping the restriction would return places outside it.
A circle restriction is searched as the box around it, results outside the circle are dropped afterwards, and the box is clamped at the poles and antimeridian, which loses only the part of the circle beyond them.
A request keeping only places of a rating, price or EV charging options is answered as no places, because og holds none of these.
Photo media is refused as Google refuses a name it never issued, because og holds no photos.
Nearby search answers nearest first under either rankPreference, since og has no popularity measure.

Rejected.

- **Drop an antimeridian-crossing restriction.** It would return places outside the box the client asked for.
- **Refuse a request with no field mask like Google.** og answers every field instead.

### HERE autocomplete and browse ask og for everything, then filter

Where. `autocomplete` and `browse` in `src/compat/here/search.rs`.

og suggests places and addresses in one list, but HERE autocomplete is addresses alone.
So og is asked for the most it gives, MAX_AUTOCOMPLETE_LIMIT, and the places are dropped afterwards.
Browse and types or route filters work the same way.
What a filter drops comes out of og's answer, so og is asked for all one search gives.
HERE's chains filter has no match in og, which carries no HERE chain.
A list that keeps only some chains returns nothing, and a list that only excludes keeps everything.

Rejected.

- **Pass the caller's limit straight to og.** Dropping places or filtered items afterwards would leave fewer results than the caller asked for.

Evidence.

MAX_AUTOCOMPLETE_LIMIT is 20.
The chains check returns an empty page when Listed::parse(params.get("chains")).included is not empty.

### HERE geocode uses og structured search only for a plain street address

Where. `Geocode::og_fields` in `src/compat/here/search.rs`.

A HERE forward geocode goes to og's structured geocode only when there is a street and no free text, no district, no county and no country given by name.
Anything else is sent as one text. og's structured form has no field for a district or a county, and text loses none of them.

Rejected.

- **Always use og's structured geocode for qualified queries.** It has no field for a district or a county, so those parts would be lost.

Evidence.

The structured branch is q none, street set, district none, county none and no country name.
Everything else goes through the q text branch.

### Mapbox autocomplete does not accept a weaker reading that drops the street

Where. `answers_whole` in `src/compat/mapbox/geocoding.rs`.

A numbered address query counts as read whole only when the number is matched or interpolated and the street is matched.
A weaker reading may match the number and city while dropping the street, and it cannot end the search.
An answer og judged on no part is left to completion, so a prefix such as Provid is completed because it may answer a street og judged on nothing.

Rejected.

- **Treat any reading that matches number and city as the whole query.** It may drop the street entirely.

Evidence.

Provid

### One house, many copies, merged within 1000 metres

Where. `House::is_same` and `one_per_house` in `src/compat/house.rs`; `DISPUTED_HOUSE_RADIUS_M` in `src/builder/osm.rs`.

Sources place one house at its door, its parcel or its building's middle, which on a large building or lot is a few hundred metres apart.
Two copies with the same street, number and unit within DISPUTED_HOUSE_RADIUS_M (1000 m) are one house.
A postcode is written one way, so two postcodes that differ mean two houses.
A town has names in several languages (Wien and Vienna), so it tells houses apart only when one copy states no postcode.
The copy kept is the most precise point, then the one stating most of its address, then the best ranked, and it takes the building any copy is drawn as, because a point at a building's door and the outline are one house.

Rejected.

- **Tell houses apart by town name.** The same town has names in several languages (Wien and Vienna).
- **A tight distance threshold.** Sources disagree by a few hundred metres on a large building or lot.

Evidence.

Wien versus Vienna.
A few hundred metres between sources on large buildings.

### Opening hours are read only in plain weekly forms

Where. `Week` and `Span` in `src/compat/hours.rs`.

Only 24/7 and rules of days and times are read, such as Mo-Fr 08:00-18:00; Sa 09:00-13:00,14:00-02:00; Su off.
A rule after a semicolon replaces earlier rules for its days and one after a comma adds to them.
A value with holidays, months, weeks, sunrise, comments or open ends states hours a week cannot hold, so it is not read at all and every vendor slot for it stays empty.
A span whose close equals its open, or is more than a day after it, is not a span.

Rejected.

- **Read the plain part of a value and ignore the rest.** The rest states hours a week cannot hold, so a partial week would be wrong.
  An empty slot is better than a wrong one.

### Vendor answers keep only the best matches and the narrowest nested area

Where. `best_matches` in `src/compat/mod.rs`.

When the first result matches every part the query states, results that match worse are dropped, since a vendor answers the address it found and not near misses.
Of the areas kept, one that holds another kept area is dropped, so the narrowest of nested matches answers.
If the first result leaves a stated part unmatched, every result stays.

Rejected.

- **Return every result og holds.** Near misses leak out, for example Back Bay Road in Newark for the query Back Bay, Boston, where the district is the answer.
- **Keep nested areas.** Paris the city should answer and not the department of the same name that holds it.
  Köln and the Cologne in Italy both stay because neither holds the other.

Evidence.

Back Bay Boston district versus Back Bay Road in Newark.
Paris city versus Paris department.
Köln and Cologne in Italy.

### Vendor error responses carry the Problem as Logged, not as a Problem extension

Where. `VendorError` in `src/compat/mod.rs`.

A vendor error inserts the Problem under the Logged extension.
The request-id middleware logs it but leaves the vendor body alone.

Rejected.

- **Insert it as a Problem extension.** The request-id middleware would rewrite the body into og's own problem document.

### Vendor ids carry the layer and label because og's id alone is ambiguous

Where. `OgRef`, `CatalogueId` and `keep_named` in `src/compat/mod.rs`.

A vendor id travels with the layer, and a record the catalogue cannot look up also carries its label, which finds it again.
An OSM node is both a POI and the address it states, and the catalogue holds no address, estimate or derived postcode, so og's id alone does not name one record.
Only a CatalogueId reaches /places?ids. keep_named keeps the record with id and label, else id, else label.
An id alone can be read otherwise, because a range the label names whole is also the house its last number names.
A label alone covers an imported row whose id a newer release renumbered.
A forged address id without its label is rejected because the catalogue never holds it.

Rejected.

- **Use og's id alone as the vendor id.** It does not name one record, since an OSM node is a POI and the address it states.

### Vendor reverse radius only widens, and the nearest-house reach is 150 m

Where. `NEAREST_HOUSE_RADIUS_M` and `radius` in `src/compat/mod.rs`.

A vendor that states no distance gets a 150 m reach for the nearest house, because vendors answer the nearest house wherever the point falls on its lot or the street before it and a lot runs to about 100 m deep.
A vendor's stated distance only widens og's search, up to the farthest og looks.
A nearer limit is the vendor's filter on what og finds, so og still answers the house and areas it would answer without one.

Rejected.

- **Narrow og's search to the vendor's smaller radius.** og would then lose the house and areas it would otherwise answer.

Evidence.

A lot runs to about 100 m deep.

## The og command

### A Pack is built from regions that do not overlap

Where. `overlaps` in `src/bin/og/regions.rs`.

A region overlaps itself and everything above or below it, and nothing beside it.
A Pack is built only from regions that are pairwise disjoint.

Rejected.

- **Allow nested regions in one Pack.** Overlapping extracts index the same places twice, so canada and canada/ontario together would hold Ontario's addresses once from each.

### Build time estimates only from a past build of similar size and format

Where. `estimate`, `ESTIMATE_RANGE` and `BuildRecord` in `src/bin/og/home.rs`.

An estimate scales the time of the finished build closest in input size, and only if it is within a factor of 4.
It must also be of the current Pack format and match on whether OpenAddresses was imported.
Otherwise there is no estimate.

Rejected.

- **Scale linearly from any past build.** Build time is not linear in input size over a wide range.
  A small build is mostly fixed costs and a large one spills to disk.
- **Use builds of another Pack format.** They did other work, so their time estimates nothing.

### Downloads for all inputs finish before any build starts

Where. `run` in `src/bin/og/job.rs`.

Every input that needs its extract is downloaded and checked before any of them is built, so a build never starts on a set it cannot finish.

Rejected.

- **Download and build each input in turn.** A later download failure would leave a build started on a set that cannot finish.

### Memory budget has a 64 MB floor

Where. `MIN_MEMORY_BUDGET_MB` in `src/main.rs`.

The memory budget option cannot go below 64 MB.
Under that, sorters spill so often that the build drowns in tiny run files.
The upper bound of 1 TiB only keeps the byte conversion far from overflow.

Rejected.

- **Allow any small memory budget.** Sorters would spill constantly and the build would be buried in tiny run files.

Evidence.

MIN_MEMORY_BUDGET_MB is 64 and MAX_MEMORY_BUDGET_MB is 1 << 20.

### Query fixtures are one compact JSON line per case

Where. `write_fixture_to_path` in `src/main.rs`.

A query fixture is written as one compact JSON object per case.
A line per case stays greppable and keeps a regenerated answer key to a readable diff.

Rejected.

- **Pretty-print the whole fixture.** 500 cases span 4,500 lines, which is hard to grep and gives noisy diffs.

Evidence.

500 cases pretty-printed span 4,500 lines.

### The sum and the download use the one URL the redirects land on

Where. `published`, `download` and `version_path` in `src/bin/og/net.rs`.

Geofabrik sends a -latest link to a dated file such as monaco-261109, or for its biggest extracts to a mirror that alone has the .md5.
The URL is resolved by following redirects once, then both the sum and the download use that one URL.
A new snapshot published between reading the sum and downloading cannot split them.
A partial download continues only while the server still reports the same ETag, or else Last-Modified, and starts over otherwise.

Rejected.

- **Read the sum beside the -latest link and download from -latest.** The biggest extracts redirect to a mirror that alone has the .md5, and a new snapshot could be published between the two requests.

Evidence.

Tests cover the biggest extracts redirecting to a mirror, and a snapshot published after the sum was read.

### og interface draws inline below the prompt and falls back to full screen

Where. `main` in `src/bin/og/main.rs`.

The interface is one window of fixed height drawn inline below the prompt, so finished steps are printed above it and stay in terminal history.
A terminal no taller than the window gets the full screen instead.
Without scrollback, what the window would have pushed up is printed on leaving.
Bracketed paste is enabled so a pasted CSV arrives in one piece, and terminals without it send keys.
An extract and the extract it is part of, such as canada and canada/ontario, or a region given twice, is refused rather than silently built, because it would index the same places twice.

Rejected.

- **Always use the full screen.** Finished steps would not stay in terminal history.
- **Silently build overlapping extracts.** The same places would be indexed twice.

## Platforms and resources

### A lookup stops itself because its blocking thread cannot be interrupted

Where. `check`, `within` and `Stop` in `src/stop.rs`.

A lookup runs on a blocking thread that nothing can interrupt from outside.
Dropping the future that waits for it drops only the answer and the thread works on.
So the lookup calls check between its steps and stops itself past its deadline or when cancelled.
The stop is held per thread because every step of a lookup runs on the thread that started it, and work handed to another thread does not see it. within restores the earlier stop also on panic, because a batch catches an item's panic and runs the next item on the same thread.

Rejected.

- **Cancel by dropping the waiting future.** It drops only the answer and the thread keeps working.

### External sort frees disk chunk by chunk because exFAT cannot punch holes

Where. `CHUNK_BYTES`, `BLOCK_BYTES` and `Cursor` in `src/extsort/block.rs`.

Runs are written to chunk files of about 8 MiB and a run read once deletes each chunk as soon as it is read, since exFAT cannot punch holes.
Small chunks keep low the partly read chunk each run still holds during a merge.
Large chunks keep the directory short, since exFAT searches it in order on every create.
Blocks of 256 KiB are large enough for zstd to find its matches and small enough for thousands of runs to hold two each.
A reader opens a chunk for each block and closes it after, so a merge of thousands of runs stays under any open file limit.

Rejected.

- **Hold files open between reads.** A merge of thousands of runs would exceed the open file limit.

### Heap counter keeps per-thread deltas and tests before storing

Where. `CountingAlloc`, `FLUSH_BYTES` and `mark_installed` in `src/alloc.rs`.

Each thread counts allocated bytes into a local delta and adds it to the shared count only once it passes 256 KiB, so the common path touches no shared cache line.
The count is behind by at most that much per thread.
The installed flag and the peak are read before they are stored, because even a store of the same value takes the cache line exclusive, every allocation of every thread passes there, and the peak rarely moves.
A thread being torn down has no local delta and counts straight into the shared total.

Rejected.

- **Update the shared counter on every allocation.** Every thread would fight over one cache line on the hottest path in the program.
- **Store the installed flag and peak unconditionally.** A store of the same value still takes the cache line exclusive.

Evidence.

FLUSH_BYTES is 256 << 10.

### Pack generations drop after every file writer inside them

Where. `Generation` in `src/pack/mod.rs`; `BuildState.generation` in `src/builder/osm.rs`.

A generation that no build keeps is removed when dropped.
Windows keeps a directory while a file in it is open, so every struct holding a generation declares it as its last field and whatever holds a file in it drops first.
The generation field is declared last in the build state so it drops last.
A failed build removes its generation's directory as it drops, and Windows keeps a directory while a file in it is still open, as the audit and the sorters' runs are until the fields before it drop.

Rejected.

- **Remove the directory in any drop order.** On Windows the removal fails while a file in the directory is still open.

Evidence.

Windows refuses to remove a directory holding open files.

### Resource probe reads what the process may use, not installed hardware

Where. `Resources::probe` in `src/resources.rs`.

Resources::probe reads the limits that bind this process.
For CPUs that is the affinity mask, cgroup quota or Windows Job object cap.
For memory it is MemAvailable, a cgroup v2 memory.max or v1 memory.limit_in_bytes less what the cgroup uses, RLIMIT_AS and RLIMIT_DATA, and on Windows the commit limit (allocations fail when it runs out however much RAM looks free) and a Job object memory limit.
Installed hardware is the wrong number, and memory in use by others is not free.
A long build probes again at a stage boundary, since it has by then used memory and page cache a fresh process would not have.
In tests, available memory is set to total memory, because many small builds run side by side and each would count the others' memory as taken, so a build's plan was refused or warned depending on what its neighbours held at that instant.

Rejected.

- **Use installed CPU and RAM.** A process may be held below that by affinity, a cgroup, a Job object, an rlimit or the commit limit.
- **Probe live free memory in tests.** Parallel test builds see each other's memory as taken, so plans flip between refused, warned and fine.

## Benchmarks and calibration

### Calibration uses held-out rows and no clock

Where. `run` in `src/calibration.rs`.

Calibration queries are built from address rows the Pack was not built from.
The searcher gets a one hour budget so there is effectively no clock, which keeps what calibration learns independent of how loaded the machine was.

Rejected.

- **Fit the confidence model on rows the Pack was built from.** Such a row is found as itself, so a model fit to it rates every answer too high.
- **Use the normal search time budget during calibration.** Results would depend on how loaded the machine was.

### Coordinate benchmark is separate from the equality benchmark

Where. `FixtureMeta` and `bucket_of` in `src/bench/geocode.rs`.

The geocoding benchmark that scores distance to an authoritative coordinate lives apart from the harness that checks whether a named object came back by osm_type and osm_id.
The first is an ordering test and the second is an equality test.
The distance buckets and geohash precisions are read from the fixture's own metadata and never derived from the run, so a later build cannot retune the scale it is measured against. hit@5 uses the best distance among the first five results, so a block of neighbouring houses cannot satisfy it by accident.
Cases whose street is not in the Pack are counted apart, and a query that returns nothing is a coverage outcome, so an OSM coverage gap is never read as a ranking failure.

Rejected.

- **Share one fixture between the equality and coordinate benchmarks.** The scoring rule would become implicit in which fields a case happened to populate.
- **Derive buckets or geohash precision from the run.** A later build could retune the scale it is measured against.

Evidence.

Geohash agreement is checked at precision 6 as an absolute grid that cannot be retuned.

### POI benchmark only expects names that are unique within their area

Where. `sample_poi_fixture` in `src/bench/pack.rs`.

A POI is drawn only if its name is unique within its locality, and POIs are counted over both the locality and the district they lie in.
A POI with no locality is not drawn.
Sampling is a seeded Fisher-Yates prefix, reproducible for one Pack and seed.

Rejected.

- **Count names within the locality only.** A query like Ali Baba, Toronto would find both when one is unique in North York but another sits elsewhere in Toronto.
  Toronto is a district with no locality over its core, so every POI in Toronto must be counted, including those in a locality inside it.

Evidence.

Tim Hortons has two in Toronto, so neither can be expected.

### The memory check adds a fixed overhead and a per-thread overhead to the budget

Where. `BuildPlan` and the memory constants in `src/builder/plan.rs`.

A build needs its memory budget plus 512 MiB plus 64 MiB for each worker thread, and the check wants that free beyond what the process already holds outside its heap.
The overhead covers allocator pages, thread stacks and read buffers that no budget counts.

Rejected.

- **Check the budget alone.** Builds then peaked above the memory the check had cleared.

Evidence.

Four extracts of 2 to 12 GB held at most 760 MiB beyond their budget with 6 workers, in 2026-11.

### The disk and node estimates before the scan cap at the planet's counts

Where. The estimate constants in `src/builder/plan.rs`.

The estimate before the scan reads item counts from input bytes at the densest input measured, and caps them at the planet's counts, as no input holds more.
A planet newer than the one counted is larger, at about the same density.
The boundary heap is sized at 4.0 B per boundary vertex, twice the 2.0 B measured on the planet (409 MB of descriptions for 203.6 million vertices), to leave room for the index of the ways' lines.
Boundary vertices per input byte was never measured. It comes from the spill study's upper estimate of 15 GB over 95.4 GB of PBF at 64 B a vertex, and the refined plan replaces it with the real count.

Rejected.

- **One ratio of items to PBF bytes for every input.** A byte of Germany holds four times the node references of a byte of Canada, so no ratio fits both.

Evidence.

The planet's counted build fit its 1.6 billion needed nodes in a 20 GiB budget and peaked at 65 GiB of scratch and Pack, so 80 GB free must pass the check.
