use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use fst::MapBuilder;

use super::{
    DICTIONARY_VERSION, Entry, Field, NameStats, count_street_ends, merge_alias, street_alias_keys,
    street_types,
};
use crate::{
    container::Writer,
    error::{Error, Result},
    extsort::{ExternalSorter, Scratch, Sorted, Spill, Spool, SpoolFile},
    memory::Budget,
    parse::lattice::CountryCode,
    util::codec::{get_str, get_u64, put_str, put_u64},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Use {
    name: String,
    country: Option<CountryCode>,
    level: u8,
    count: u64,
}

impl Spill for Use {
    fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, &self.name);
        put_u64(out, country_value(self.country));
        put_u64(out, u64::from(self.level));
        put_u64(out, self.count);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            name: get_str(input)?.to_string(),
            country: country_of(get_u64(input)?)?,
            level: u8::try_from(get_u64(input)?).map_err(|_| Error::Decode("a level past u8"))?,
            count: get_u64(input)?,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.name.capacity()
    }
}

/// `seq` keeps the merges of one alias in street order.
#[derive(Debug, Clone)]
struct Named {
    name: String,
    seq: u64,
    entry: Entry,
}

impl PartialEq for Named {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Named {}

impl PartialOrd for Named {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Named {
    fn cmp(&self, other: &Self) -> Ordering {
        (&self.name, self.seq).cmp(&(&other.name, other.seq))
    }
}

impl Spill for Named {
    fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, &self.name);
        put_u64(out, self.seq);
        put_u64(out, self.entry.count);
        put_u64(out, country_value(self.entry.country));
        put_u64(out, u64::from(self.entry.levels));
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            name: get_str(input)?.to_string(),
            seq: get_u64(input)?,
            entry: Entry {
                count: get_u64(input)?,
                country: country_of(get_u64(input)?)?,
                levels: u8::try_from(get_u64(input)?)
                    .map_err(|_| Error::Decode("levels past u8"))?,
            },
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.name.capacity()
    }
}

fn country_value(country: Option<CountryCode>) -> u64 {
    country.map_or(0, |country| u64::from(country.to_u16()))
}

fn country_of(value: u64) -> Result<Option<CountryCode>> {
    match value {
        0 => Ok(None),
        value => u16::try_from(value)
            .ok()
            .and_then(CountryCode::from_u16)
            .map(Some)
            .ok_or(Error::Decode("not a country code")),
    }
}

#[derive(Debug)]
struct Uses {
    sorter: ExternalSorter<Use>,
    held: Option<Use>,
}

impl Uses {
    fn new(
        scratch: &Arc<Scratch>,
        label: &'static str,
        memory: &Arc<Budget>,
        share: usize,
    ) -> Self {
        Self {
            sorter: ExternalSorter::new(scratch, label, memory, share),
            held: None,
        }
    }

    fn add(
        &mut self,
        name: String,
        country: Option<CountryCode>,
        level: u8,
        count: u64,
    ) -> Result<()> {
        if let Some(held) = &mut self.held
            && held.name == name
            && held.country == country
            && held.level == level
        {
            held.count += count;
            return Ok(());
        }
        let next = Use {
            name,
            country,
            level,
            count,
        };
        match self.held.replace(next) {
            Some(held) => self.sorter.push(held),
            None => Ok(()),
        }
    }

    fn finish(mut self) -> Result<Stats> {
        if let Some(held) = self.held.take() {
            self.sorter.push(held)?;
        }
        Ok(Stats {
            uses: self.sorter.finish()?,
            next: None,
        })
    }
}

struct Stats {
    uses: Sorted<Use>,
    next: Option<Use>,
}

impl Iterator for Stats {
    type Item = Result<(String, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        let first = match self.next.take() {
            Some(first) => first,
            None => match self.uses.next()? {
                Ok(first) => first,
                Err(error) => return Some(Err(error)),
            },
        };
        let mut stats = NameStats::default();
        stats.add(first.count, first.country);
        stats.levels |= first.level;
        for item in self.uses.by_ref() {
            let item = match item {
                Ok(item) => item,
                Err(error) => return Some(Err(error)),
            };
            if item.name != first.name {
                self.next = Some(item);
                break;
            }
            stats.add(item.count, item.country);
            stats.levels |= item.level;
        }
        Some(Ok((first.name, stats.entry())))
    }
}

#[derive(Debug)]
pub(super) struct Streamed {
    scratch: Arc<Scratch>,
    memory: Arc<Budget>,
    share: usize,
    streets: Uses,
    pois: Uses,
}

impl Streamed {
    pub(super) const FIELDS: [Field; 2] = [Field::Street, Field::Poi];

    pub(super) fn new(scratch: &Arc<Scratch>, memory: &Arc<Budget>, share: usize) -> Self {
        Self {
            scratch: Arc::clone(scratch),
            memory: Arc::clone(memory),
            share,
            streets: Uses::new(scratch, "dictionary-streets", memory, share),
            pois: Uses::new(scratch, "dictionary-pois", memory, share),
        }
    }

    pub(super) fn add(
        &mut self,
        field: Field,
        name: String,
        country: Option<CountryCode>,
        level: u8,
    ) -> Result<()> {
        match field {
            Field::Poi => self.pois.add(name, country, level, 1),
            _ => self.streets.add(name, country, level, 1),
        }
    }

    pub(super) fn finish(self) -> Result<Finished> {
        let Self {
            scratch,
            memory,
            share,
            streets,
            pois,
        } = self;
        let mut words = Uses::new(&scratch, "dictionary-words", &memory, share);
        let mut spool = Spool::create(&scratch, "dictionary-streets")?;
        let mut ends = HashMap::new();
        let mut names = 0;
        for named in streets.finish()? {
            let (name, entry) = named?;
            count_words(&mut words, &name)?;
            count_street_ends(&mut ends, &name);
            spool.write(&Named {
                name,
                seq: names,
                entry,
            })?;
            names += 1;
        }
        let streets = spool.finish()?;
        let types = street_types(ends, names);
        let mut bare = ExternalSorter::new(&scratch, "dictionary-aliases", &memory, share);
        let mut tail = ExternalSorter::new(&scratch, "dictionary-aliases", &memory, share);
        for street in streets.reader()? {
            let Named { name, seq, entry } = street?;
            let (named, lasts) = street_alias_keys(&name, &types);
            if let Some(named) = named {
                bare.push(Named {
                    name: named,
                    seq,
                    entry,
                })?;
            }
            for last in lasts {
                tail.push(Named {
                    name: last,
                    seq,
                    entry,
                })?;
            }
        }
        let bare = merged(&scratch, bare.finish()?, &mut words)?;
        let tail = merged(&scratch, tail.finish()?, &mut words)?;
        Ok(Finished {
            types,
            streets,
            pois: Some(pois),
            words: Some(words),
            bare,
            tail,
        })
    }
}

fn count_words(words: &mut Uses, name: &str) -> Result<()> {
    for word in name.split_whitespace() {
        words.add(word.to_string(), None, 0, 1)?;
    }
    Ok(())
}

fn merged(
    scratch: &Arc<Scratch>,
    sorted: Sorted<Named>,
    words: &mut Uses,
) -> Result<SpoolFile<Named>> {
    let mut spool = Spool::create(scratch, "dictionary-aliases")?;
    let mut held: Option<Named> = None;
    for named in sorted {
        let named = named?;
        match &mut held {
            Some(alias) if alias.name == named.name => merge_alias(&mut alias.entry, named.entry),
            _ => {
                if let Some(alias) = held.replace(named) {
                    count_words(words, &alias.name)?;
                    spool.write(&alias)?;
                }
            }
        }
    }
    if let Some(alias) = held {
        count_words(words, &alias.name)?;
        spool.write(&alias)?;
    }
    spool.finish()
}

pub(super) struct Finished {
    types: BTreeMap<String, Entry>,
    streets: SpoolFile<Named>,
    pois: Option<Uses>,
    words: Option<Uses>,
    bare: SpoolFile<Named>,
    tail: SpoolFile<Named>,
}

impl Finished {
    pub(super) fn types(&self) -> &BTreeMap<String, Entry> {
        &self.types
    }

    /// `false` when `field`'s names are not held here. `held` are its names held in memory.
    pub(super) fn write(
        &mut self,
        field: Field,
        held: &BTreeMap<String, Entry>,
        pack: &mut Writer,
    ) -> Result<bool> {
        let entries: Box<dyn Iterator<Item = Result<(String, Entry)>>> = match field {
            Field::Street => Box::new(spooled(&self.streets)?),
            Field::StreetBare => Box::new(spooled(&self.bare)?),
            Field::StreetTail => Box::new(spooled(&self.tail)?),
            Field::Poi => Box::new(taken(&mut self.pois)?.finish()?),
            Field::Word => Box::new(with_held(taken(&mut self.words)?.finish()?, held)),
            _ => return Ok(false),
        };
        pack.begin(field.section(), DICTIONARY_VERSION)?;
        let mut builder = MapBuilder::new(&mut *pack)?;
        for entry in entries {
            let (key, entry) = entry?;
            builder.insert(key, entry.encode())?;
        }
        builder.finish()?;
        pack.end()?;
        Ok(true)
    }
}

fn spooled(spool: &SpoolFile<Named>) -> Result<impl Iterator<Item = Result<(String, Entry)>>> {
    Ok(spool
        .reader()?
        .map(|named| named.map(|named| (named.name, named.entry))))
}

fn with_held<'a>(
    sorted: impl Iterator<Item = Result<(String, Entry)>> + 'a,
    held: &'a BTreeMap<String, Entry>,
) -> impl Iterator<Item = Result<(String, Entry)>> + 'a {
    let mut sorted = sorted.peekable();
    let mut held = held.iter().peekable();
    std::iter::from_fn(move || {
        let order = match (sorted.peek(), held.peek()) {
            (Some(Ok((word, _))), Some((held, _))) => held.as_str().cmp(word),
            (None, Some(_)) => Ordering::Less,
            (_, None) | (Some(Err(_)), _) => Ordering::Greater,
        };
        match order {
            Ordering::Greater => sorted.next(),
            Ordering::Less => held.next().map(|(word, entry)| Ok((word.clone(), *entry))),
            Ordering::Equal => {
                let count = held.next().map_or(0, |(_, entry)| entry.count);
                sorted.next().map(|word| {
                    word.map(|(word, mut entry)| {
                        entry.count += count;
                        (word, entry)
                    })
                })
            }
        }
    })
}

fn taken(uses: &mut Option<Uses>) -> Result<Uses> {
    uses.take()
        .ok_or_else(|| Error::Inconsistent("a dictionary section written twice".into()))
}
