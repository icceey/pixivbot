// Placeholder for Pixiv API models
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct Illustrator {
    pub id: u64,
    pub name: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct Illustration {
    pub id: u64,
    pub title: String,
    pub urls: Vec<String>,
}
