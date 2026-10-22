# open-geocode

A self-hosted geocoder: it turns written addresses and place names into locations, and locations back into addresses, from OpenStreetMap and other open data.

## Data

**Pack**:
One file holding everything og answers from: the records, their text index and their spatial index, built once from an extract.
_Avoid_: database, index (alone), dataset

**Record**:
One thing a Pack can answer with: an address, a street, a place, a postcode or a point of interest.
_Avoid_: document, entry, feature

**Layer**:
The kind of thing a record is: address, street, interpolation, postcode, locality, region, country or POI.
_Avoid_: type, category

**Place type**:
The size class the source gave a place (city, town, village, hamlet, or an administrative level), which breaks ties between places of the same name.
_Avoid_: importance, population

**Disputed address**:
An address that another source places farther away than sources placing one house disagree, though not so far it could be another house.
The Pack keeps one record of it, at OSM's point when OSM has it, and ranks and rates it below an address no source disputes.
_Avoid_: duplicate, conflict

**Street postcodes**:
The postcodes the other addresses of a street state, lent to an address of that street that states none.
_Avoid_: inferred postcode

## Reading a query

**Reading**:
One way the parser labels the words of a query, as street, house number, unit, locality, region, postcode or country; the parser offers its best few, best first.
_Avoid_: parse, interpretation, labelling

**Subject**:
The words of a reading that say what the sought record is: its street, or its name.
_Avoid_: required words

**Unplaced word**:
A query word the parser could not label; it ranks the answers, and only a query with no subject needs most of its unplaced words matched.
_Avoid_: other, unknown token, catch-all

**Stated area**:
A locality, region, postcode or country the query names as where the sought record is.
_Avoid_: context, area (alone), locality part

**Known area word**:
A word of a stated area that names some area the Pack holds; a stated area made only of words the Pack does not know is ranked on, never required.
_Avoid_: recognised place

## Matching

**Search key**:
A word as og matches it: in compatibility form, lower case, with Latin letters folded to plain ASCII ("Łódź" is "lodz", "Straße" is "strasse").
_Avoid_: normalised text, folded text

**Exact key**:
A word as written, in lower case with its accents kept, which breaks ties in favour of the spelling the query used.
_Avoid_: original text

**Typo tier**:
How many edits from its written form a query word may match: none, then one, then two, each tried only when the ones before found nothing; numbers are never matched with a typo.
_Avoid_: fuzziness, fuzzy level

**Pass**:
How strictly a reading is searched: strict (the house number, in the stated localities), street (a nearby number or the street itself standing in for it) and lenient (the stated localities only rank).
_Avoid_: mode, attempt

**Agreement**:
How a record relates to a stated area or postcode: it agrees, it contradicts, or nothing says either way.
A contradicting locality removes the record; in search, a contradicting postcode ranks it last.
_Avoid_: match, mismatch, verdict

**Focus**:
Where the caller says they are, a point that breaks ties between answers the query cannot tell apart, nearer first in widening distance bands.
It never outranks a word of the query, and a stated area is never relaxed for it.
_Avoid_: location bias, proximity boost

**Evidence**:
What makes one reading's answers outrank another's: the stricter pass that found them first, then the parser's confidence in the reading.
_Avoid_: score (alone), confidence (alone)

**Stand-in**:
A word answering for something it does not name: a street for a house number no record states, or a subject word for an area the Pack does not know.
_Avoid_: fallback

**Confidence**:
How often an answer like this one is the record its query asks for, from 0 to 1, comparable across queries.
It is read from what search knew of the answer (its pass, typo tier, accuracy, agreement, and its margin over the next answer) by a model fit per Pack to held-out addresses; a Pack never calibrated gives none.
_Avoid_: score, probability (alone)
