//! QRank (<https://qrank.toolforge.org>, CC0): Wikimedia page views per
//! Wikidata item, `Entity,QRank` rows such as `Q5296,4958942240`.

use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
    sync::Arc,
};

use crate::{
    error::{Error, IoPathExt, Result},
    memory::{Budget, Reservation},
};

const ITEM_BYTES: usize = std::mem::size_of::<u32>() + std::mem::size_of::<u8>();

#[derive(Debug)]
pub struct QRank {
    items: Vec<u32>,
    importance: Vec<u8>,
    _memory: Reservation,
}

impl QRank {
    pub fn load(path: &Path, budget: &Arc<Budget>) -> Result<Self> {
        let file = File::open(path).at("failed to open", path)?;
        let mut reader = BufReader::new(file);
        let gzipped = reader.fill_buf()?.starts_with(&[0x1f, 0x8b]);
        let reader: Box<dyn Read> = if gzipped {
            Box::new(flate2::read::MultiGzDecoder::new(reader))
        } else {
            Box::new(reader)
        };
        Self::read(BufReader::new(reader), budget)
    }

    fn read(reader: impl BufRead, budget: &Arc<Budget>) -> Result<Self> {
        let mut rows = Vec::new();
        let mut memory = Reservation::empty(budget);
        for (number, line) in reader.lines().enumerate() {
            let line = line?;
            if number == 0 && line.starts_with("Entity") {
                continue;
            }
            let (entity, rank) = line
                .split_once(',')
                .ok_or(Error::Decode("a QRank row is not Entity,QRank"))?;
            let (Some(item), Ok(rank)) = (
                entity
                    .strip_prefix('Q')
                    .and_then(|item| item.parse::<u32>().ok()),
                rank.trim().parse::<u64>(),
            ) else {
                return Err(Error::Decode("a QRank row is not Entity,QRank"));
            };
            if rows.len() == rows.capacity() {
                memory.force_grow(rows.capacity().max(1024) * ITEM_BYTES);
            }
            rows.push((item, importance(rank)));
        }
        rows.sort_unstable_by_key(|(item, _)| *item);
        rows.dedup_by_key(|(item, _)| *item);
        let (items, importance) = rows.into_iter().unzip();
        Ok(Self {
            items,
            importance,
            _memory: memory,
        })
    }

    pub fn importance(&self, item: u32) -> u8 {
        self.items
            .binary_search(&item)
            .map_or(0, |index| self.importance[index])
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

// 7.5 steps a doubling: the most read item, ~5 billion views, still fits a byte.
fn importance(rank: u64) -> u8 {
    let doublings = (rank as f64 + 1.0).log2();
    (1.0 + doublings * 7.5).clamp(1.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_items_by_their_page_views_on_a_log_scale() {
        let csv = "Entity,QRank\nQ5296,4958942240\nQ90,1000000\nQ7,0\n";
        let qrank = QRank::read(csv.as_bytes(), &Budget::unlimited()).expect("qrank");
        assert_eq!(qrank.len(), 3);
        assert!(qrank.importance(5296) > qrank.importance(90));
        assert!(qrank.importance(90) > qrank.importance(7));
        assert_eq!(qrank.importance(7), 1, "ranked, however little read");
        assert_eq!(qrank.importance(8), 0, "not ranked");
        assert!(QRank::read("Q1;5\n".as_bytes(), &Budget::unlimited()).is_err());
    }
}
