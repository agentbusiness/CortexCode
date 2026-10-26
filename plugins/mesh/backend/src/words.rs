//! The pairing code: four words from a list of 256 (32 bits), typed on the other computer. The
//! words are short, distinct and easy to say aloud.

use rand::RngCore;

pub const WORDS: [&str; 256] = [
    "acid", "acorn", "actor", "amber", "angel", "ankle", "apple", "apron", "arrow", "atlas", "badge", "bagel", "baker", "basil", "beach", "beard",
    "berry", "bison", "blade", "blaze", "bloom", "board", "brick", "bride", "broom", "brush", "cabin", "cable", "camel", "candy", "canoe", "cargo",
    "cedar", "chalk", "cheek", "chess", "chief", "chili", "chimp", "cider", "cigar", "clock", "cloud", "clown", "cobra", "cocoa", "comet", "coral",
    "couch", "crane", "crate", "crown", "cycle", "daisy", "dance", "delta", "diner", "diver", "donut", "dream", "drum", "eagle", "earth", "ember",
    "fable", "fairy", "feast", "fence", "ferry", "fiber", "field", "flame", "flute", "forge", "frost", "fudge", "gecko", "ghost", "giant", "glass",
    "globe", "glove", "goose", "grape", "grass", "guard", "guide", "hawk", "heron", "hobby", "honey", "horse", "hotel", "house", "igloo", "image",
    "india", "ivory", "jelly", "jewel", "juice", "kayak", "kiosk", "kite", "koala", "lemon", "lever", "light", "lilac", "lion", "llama", "lotus",
    "lunar", "lunch", "magic", "mango", "maple", "marsh", "medal", "melon", "metal", "mimic", "mocha", "moose", "motor", "mouse", "music", "nacho",
    "needle", "noble", "north", "novel", "nurse", "oasis", "ocean", "olive", "onion", "opera", "orbit", "otter", "oxen", "paint", "panda", "paper",
    "pasta", "patch", "peach", "pearl", "pedal", "penny", "piano", "pilot", "pixel", "pizza", "plant", "plaza", "plum", "poem", "polar", "pond",
    "prism", "pulse", "puppy", "queen", "quilt", "radar", "radio", "raven", "ridge", "river", "robin", "robot", "rocket", "roost", "rose", "ruby",
    "rugby", "salad", "salsa", "sauce", "scarf", "scout", "shark", "sheep", "shell", "shore", "silk", "siren", "skate", "slate", "smile", "snail",
    "snake", "solar", "sonar", "spice", "spoon", "spray", "squid", "stamp", "stone", "storm", "straw", "sugar", "sunny", "swan", "sword", "table",
    "taco", "tango", "teeth", "tiger", "toast", "token", "topaz", "torch", "tower", "train", "trout", "truck", "trunk", "tulip", "tuna", "turbo",
    "uncle", "union", "urban", "vapor", "vault", "velvet", "video", "vinyl", "viola", "vivid", "wagon", "walnut", "water", "whale", "wheat", "wheel",
    "wolf", "world", "yacht", "yeast", "yogurt", "zebra", "zinc", "zone", "pine", "oak", "elm", "fern", "moss", "reed", "sage", "mint",
];

/// Four random words joined with dashes.
pub fn code() -> String {
    let mut bytes = [0_u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| WORDS[*b as usize]).collect::<Vec<_>>().join("-")
}

/// The words of a typed code, lower-cased, separators tolerated (dashes, spaces, commas).
pub fn normalize(code: &str) -> String {
    code.split(|c: char| c == '-' || c.is_whitespace() || c == ',').filter(|word| !word.is_empty()).map(str::to_lowercase).collect::<Vec<_>>().join("-")
}
