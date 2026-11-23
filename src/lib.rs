mod address_template;
pub mod alloc;
pub mod batch;
pub mod bench;
pub mod builder;
pub mod calibration;
pub mod categories;
mod compat;
#[cfg(test)]
mod contract_tests;
pub mod error;
pub mod extsort;
mod http;
pub mod labels;
pub mod licences;
pub mod pack;
pub mod parse;
pub mod record;
pub mod resources;
pub mod response;
pub mod reverse;
pub mod search;
mod stop;
mod util;

pub(crate) use self::address_template::lines as address_lines;
pub use self::bench::geocode as geocode_bench;
pub use self::builder::{addresses::openaddresses, memory};
pub use self::http::{limits, server as runtime};
pub(crate) use self::pack::tiger;
pub use self::pack::{
    container, context, enclosing, ids, record_store as records, spatial_index, text_index,
};
pub use self::response as api;
pub use self::search::{confidence, rank, structured};
pub(crate) use self::util::iso;
pub use error::{Error, Result};
