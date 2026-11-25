use std::sync::Arc;

use crate::{
    error::{Error, Result},
    extsort::{IndexedSpool, IndexedSpoolFile, IndexedState, Scratch, Spill},
    util::codec::{get_u64, put_u64},
};

const CHUNK_ITEMS: usize = 64;

pub(crate) trait Entry: Spill {
    fn encode_following(&self, previous: Option<&Self>, out: &mut Vec<u8>) {
        let _ = previous;
        self.encode(out);
    }

    fn decode_following(previous: Option<&Self>, input: &mut &[u8]) -> Result<Self> {
        let _ = previous;
        Self::decode(input)
    }
}

struct Chunk<T>(Vec<T>);

impl<T: Entry> Spill for Chunk<T> {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.0.len() as u64);
        let mut previous = None;
        for item in &self.0 {
            item.encode_following(previous, out);
            previous = Some(item);
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let count = usize::try_from(get_u64(input)?)?;
        if count > CHUNK_ITEMS {
            return Err(Error::Decode("book chunk holds too many items"));
        }
        let mut items: Vec<T> = Vec::with_capacity(count);
        for _ in 0..count {
            let item = T::decode_following(items.last(), input)?;
            items.push(item);
        }
        Ok(Self(items))
    }
}

pub(crate) struct Book<T> {
    spool: IndexedSpool<Chunk<T>>,
    chunk: Vec<T>,
    len: u64,
}

impl<T: Entry> Book<T> {
    pub(crate) fn create(scratch: &Arc<Scratch>, label: &str) -> Result<Self> {
        Ok(Self {
            spool: IndexedSpool::create(scratch, label)?,
            chunk: Vec::with_capacity(CHUNK_ITEMS),
            len: 0,
        })
    }

    pub(crate) fn next_position(&self) -> u64 {
        self.len
    }

    pub(crate) fn push(&mut self, item: T) -> Result<u64> {
        self.chunk.push(item);
        self.len += 1;
        if self.chunk.len() == CHUNK_ITEMS {
            self.spool.write(&Chunk(std::mem::take(&mut self.chunk)))?;
        }
        Ok(self.len - 1)
    }

    pub(crate) fn finish(mut self) -> Result<BookFile<T>> {
        if !self.chunk.is_empty() {
            self.spool.write(&Chunk(std::mem::take(&mut self.chunk)))?;
        }
        Ok(BookFile {
            file: self.spool.finish()?,
        })
    }
}

pub(crate) struct BookFile<T> {
    file: IndexedSpoolFile<Chunk<T>>,
}

impl<T> BookFile<T> {
    pub(crate) fn state(&mut self, scratch: &Arc<Scratch>) -> Result<IndexedState> {
        self.file.state(scratch)
    }

    pub(crate) fn files(&self) -> Vec<std::path::PathBuf> {
        self.file.files()
    }

    pub(crate) fn restore(scratch: &Arc<Scratch>, state: &IndexedState) -> Result<Self> {
        Ok(Self {
            file: IndexedSpoolFile::restore(scratch, state)?,
        })
    }
}

impl<T: Entry> BookFile<T> {
    pub(crate) fn chunk_of(at: u64) -> usize {
        (at / CHUNK_ITEMS as u64) as usize
    }

    pub(crate) fn chunk(&self, index: usize) -> Result<Vec<T>> {
        if index >= self.file.len() {
            return Err(Error::Decode("book position is past its end"));
        }
        Ok(self.file.get(index)?.0)
    }

    pub(crate) fn pick(items: &[T], at: u64) -> Result<&T> {
        items
            .get((at % CHUNK_ITEMS as u64) as usize)
            .ok_or(Error::Decode("book position is past its chunk"))
    }

    pub(crate) fn items(&self) -> impl Iterator<Item = Result<T>> + '_ {
        (0..self.file.len()).flat_map(move |index| match self.chunk(index) {
            Ok(items) => items.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(error) => vec![Err(error)],
        })
    }
}

impl<T> std::fmt::Debug for BookFile<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("BookFile").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Number(u64);

    impl Spill for Number {
        fn encode(&self, out: &mut Vec<u8>) {
            put_u64(out, self.0);
        }

        fn decode(input: &mut &[u8]) -> Result<Self> {
            Ok(Self(get_u64(input)?))
        }
    }

    impl Entry for Number {
        fn encode_following(&self, previous: Option<&Self>, out: &mut Vec<u8>) {
            put_u64(out, self.0 - previous.map_or(0, |previous| previous.0));
        }

        fn decode_following(previous: Option<&Self>, input: &mut &[u8]) -> Result<Self> {
            Ok(Self(
                get_u64(input)? + previous.map_or(0, |previous| previous.0),
            ))
        }
    }

    #[test]
    fn reads_items_back_by_position_and_in_order() {
        let dir = TempDir::new("book");
        let scratch = Scratch::create(dir.join("scratch")).expect("scratch");
        let mut book = Book::create(&scratch, "book").expect("book");
        let count = CHUNK_ITEMS as u64 * 3 + 5;
        for value in 0..count {
            assert_eq!(book.push(Number(value * 7)).expect("push"), value);
        }
        let book = book.finish().expect("finish");
        for at in [0, 1, CHUNK_ITEMS as u64, count - 1] {
            let chunk = book.chunk(BookFile::<Number>::chunk_of(at)).expect("chunk");
            assert_eq!(BookFile::pick(&chunk, at).expect("pick"), &Number(at * 7));
        }
        assert!(
            book.chunk(BookFile::<Number>::chunk_of(count + CHUNK_ITEMS as u64))
                .is_err()
        );
        let all = book.items().collect::<Result<Vec<_>>>().expect("items");
        assert_eq!(
            all,
            (0..count)
                .map(|value| Number(value * 7))
                .collect::<Vec<_>>()
        );
    }
}
