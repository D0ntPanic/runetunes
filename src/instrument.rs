use crate::data::Data;
use anyhow::Result;

pub struct Instrument {
    pub data: Vec<u8>,
}

impl Instrument {
    pub fn new(data: &Data, id: u32) -> Result<Self> {
        let contents = data.instruments_archive.get(id as usize, 0)?;

        Ok(Instrument { data: contents })
    }
}
