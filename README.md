# ronin-rs 🥷

An embedded, high-performance, log-structured key-value store written in Rust. 

`ronin-rs` is an educational purpose implementation of [Bitcask](https://riak.com/assets/bitcask-intro.pdf). It provides predictable, high-throughput read and write performance by using append-only log files backed by an in-memory directory for O(1) value lookups.

## Features

- **High Write Throughput:** All writes are append-only operations, completely avoiding random disk seeks.
- **Fast Reads:** Reads require a single disk seek at most, thanks to the in-memory structure called `KeyDir`.
- **Crash Reliability:** Append-only logs mean the database state is much more resilient to crashes and power losses.
- **Data Integrity:** Every record is protected by a CRC32 checksum.
- **Automatic File Rotation:** Active data files are automatically rotated when they reach a configured threshold (defaults to 2GB).

## Usage

Here is a basic example of how to use `ronin-rs`:

```rust
use ronin_rs::Ronin;

fn main() {
    // Initialize the database in a local directory
    // This will create the directory if it doesn't exist.
    let mut db = Ronin::new("./data");

    // Store a key-value pair
    let k = b"user".to_vec();
    let v = b"ronin-rs".to_vec();
    db.put(k.clone(), v);

    // Retrieve the value
    if let Some(retrieved_val) = db.get(&k) {
        let val_str = String::from_utf8(retrieved_val).unwrap();
        println!("Found value: {}", val_str);
    } else {
        println!("Key not found.");
    }

    // Delete the key
    db.delete(&key);
}
```

## Internal Architecture

### On-Disk Format
Data is stored in log files (e.g., `bitcask-0.rn`, `bitcask-1.rn`). Every write (`put` or `delete`) appends a new entry into the currently active file. To mutate the data, we never overwrite in place; we simply append a newer version of the record.

An entry on disk looks like this:

| CRC (4 bytes) | Timestamp (8 bytes) | Key Size (8 bytes) | Value Size (8 bytes) | Key (variable byte array) | Value (variable byte array) |
| ------------- | ------------------- | ------------------ | -------------------- | ------------------------- | --------------------------- |

*Note: Deletions are represented by appending a new record for the key with an empty value (`value_sz = 0`).*

### In-Memory Directory (`KeyDir`)
To make reads lighting fast, `ronin-rs` maintains an in-memory hash map. This map links every known key to a `KeyDirEntry` containing the metadata of its most recent location on disk: File ID, Value Size, Value Offset, and Timestamp. 

When a `get` is called, `ronin-rs` figures out exactly which byte index `val_offset` the required value starts at, jumps directly to that byte, and reads exactly `value_sz` (which is the length of the value that was added) bytes.

## Development & Testing

To test the database locally without polluting your project directory, tests are written using the `tempfile` crate to automatically clean up database log files.

```bash
cargo test
```

<!-- ## Roadmap

Future improvements planned for `ronin-rs`:
- [ ] **Compaction & Merge:** Implement a background process to merge old, read-only segment files and reclaim disk space from deleted/overwritten keys.
- [ ] **KeyDir Rebuild & Hint Files:** Read existing logs to rebuild the `KeyDir` on startup, and generate Bitcask hint files for faster restart times.
- [ ] **Concurrency:** Add `Arc<RwLock<...>>` or equivalent mechanisms to support concurrent reads and writes across multiple threads safely. -->
