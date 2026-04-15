use std::collections::HashMap;
use std::fs::{File, OpenOptions, read};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::vec;
use crc32fast::Hasher;

// Every Active file threshold.
const MAX_FILE_SIZE: u32 = 2 * 1073741824;
// struct Key(Vec<u8>);

// Format to be sent into the Bitcask file.
struct DataFormat {
    crc: u32,
    timestamp: u64,
    key_sz: u64,
    value_sz: u64,
    key: Vec<u8>,
    value: Vec<u8>,
}

// KeyDirectory 
struct KeyDirEntry {
    file_id: u32,
    value_sz: u32,
    value_pos: u64,
    timestamp: u64,
}

// Appending the DataFormat entry to the current open Bitcask file, an in-memory structure called keyDir
// is updated.
// TODO: In memory structure called keyDir
struct KeyDir(HashMap<Vec<u8>, KeyDirEntry>);

pub struct Ronin {
    key_dir: KeyDir,
    active_file: File,
    active_file_id: u32,
    dir_path: PathBuf,
}

pub struct Opts {
    pub read_only: bool,
    pub sync_on_put: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            read_only: false,
            sync_on_put: false,
        }
    }
}

impl Ronin {
    pub fn new<P: AsRef<Path>>(dir_path: P) -> Self {
        let dir_path_buf = dir_path.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir_path_buf).unwrap();

        let active_file_path = dir_path_buf.join("bitcask-0.rn");

        let active_file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(active_file_path)
            .expect("Failed to open or create the active database file");

        let mut memory_map = HashMap::new();
        let mut reader = active_file.try_clone().expect("Failed to clone file handle for reading");

        reader.seek(SeekFrom::Start(0)).unwrap();

        loop {
            let entry_pos = reader.stream_position().unwrap();
            let mut header_buff = [0u8; 28];

            if let Err(e) = reader.read_exact(&mut header_buff) {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    break;
                } else {
                    panic!("Corrupted database file! Error reading headers: {}", e);
                }
            }

            // Extract pieces using the exact byte slices
            // Note that CRC is 0..4
            let timestamp = u64::from_le_bytes(header_buff[4..12].try_into().unwrap());
            let key_sz = u64::from_le_bytes(header_buff[12..20].try_into().unwrap());
            let value_sz = u64::from_le_bytes(header_buff[20..28].try_into().unwrap());

            let mut key = vec![0u8; key_sz as usize];
            reader.read_exact(&mut key).unwrap();
            reader.seek(SeekFrom::Current(value_sz as i64)).unwrap();

            if value_sz == 0 {
                memory_map.remove(&key);
            } else {
                let entry = KeyDirEntry {
                    file_id: 0,
                    value_sz: value_sz as u32,
                    value_pos: entry_pos,
                    timestamp: timestamp,
                };
                memory_map.insert(key, entry);
            }
        }

        let key_dir = KeyDir(memory_map);

        Ronin {
            key_dir,
            active_file,
            active_file_id: 0,
            dir_path: dir_path_buf,
        }
    }

pub fn open<P: AsRef<Path>>(dir_name: P, _opts: Opts) -> Self {
        // Calling new for now, since it mimics open logic.
        Self::new(dir_name)
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Buffer that holds the byte form of DataFormat fields
        let buffer_sz = 4 + 8 + 8 + 8 + key.len() + value.len();
        let mut entry_buffer: Vec<u8> = Vec::with_capacity(buffer_sz);
        let key_sz = key.len() as u64;
        let value_sz = value.len() as u64;

        let timestamp_byte = current_time.to_le_bytes();
        let key_sz_bytes = key_sz.to_le_bytes();
        let value_sz_bytes = value_sz.to_le_bytes();

        entry_buffer.extend_from_slice(&[0; 4]);
        entry_buffer.extend_from_slice(&timestamp_byte);
        entry_buffer.extend_from_slice(&key_sz_bytes);
        entry_buffer.extend_from_slice(&value_sz_bytes);
        entry_buffer.extend_from_slice(&key);
        entry_buffer.extend(&value);

        let mut hasher = Hasher::new();
        hasher.update(&entry_buffer[4..]);

        let checksum = hasher.finalize();

        entry_buffer[0..4].copy_from_slice(&checksum.to_le_bytes());
        let mut current_pos = self.active_file.stream_position().unwrap();

        if current_pos + (entry_buffer.len() as u64) > MAX_FILE_SIZE as u64 {
            self.active_file_id += 1;
            let new_file_path = self.dir_path.join(format!("bitcask-{}.rn", self.active_file_id));

            let new_file = OpenOptions::new()
                .read(true)
                .create(true)
                .append(true)
                .open(new_file_path)
                .unwrap();

            self.active_file = new_file;
            current_pos = 0;
        }
        self.active_file.write_all(&entry_buffer).unwrap();

        let entry = KeyDirEntry {
            file_id: self.active_file_id,
            value_sz: value_sz as u32,
            value_pos: current_pos,
            timestamp: current_time,
        };

        self.key_dir.0.insert(key.clone(), entry);
    }

    pub fn get(&mut self, key: &Vec<u8>) -> Option<Vec<u8>> {
        let entry = self.key_dir.0.get(key)?;
        let val_position = entry.value_pos;
        let val_size = entry.value_sz;
        let val_offset = val_position + 4 + 8 + 8 + 8 + (key.len() as u64);

        let mut val_buffer= vec![0u8; val_size as usize];
        
        if entry.file_id == self.active_file_id {
            self.active_file.seek(SeekFrom::Start(val_offset)).unwrap();
            self.active_file.read_exact(&mut val_buffer).unwrap();
        } else {
            let old_file_path = self.dir_path.join(format!("bitcask-{}.rn", entry.file_id));

            let mut old_file = File::open(old_file_path).unwrap();
            old_file.seek(SeekFrom::Start(val_offset)).unwrap();
            old_file.read_exact(&mut val_buffer).unwrap();
        }

        Some(val_buffer)
    }

    pub fn delete(&mut self, key: &Vec<u8>) {
        self.put(key.clone(), Vec::new());
        self.key_dir.0.remove(key);
    }

    pub fn list_keys(&mut self) -> Vec<Vec<u8>> {
        self.key_dir.0.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_put_retrieves_correct_data() {
        let mut db = Ronin::new("bitcask");

        let key1 = b"kenechukwu".to_vec();
        let val1 = b"ifediora".to_vec();
        db.put(key1.clone(), val1.clone());

        let retrieved = db.get(&key1);

        assert_eq!(retrieved, Some(val1));
    }

    #[test]
    fn test_delete_removes_key() {
        let mut db = Ronin::new("bitcask");

        let key = b"Shinske".to_vec();
        let val = b"Nakamura".to_vec();

        db.put(key.clone(), val.clone());
        assert_eq!(db.get(&key), Some(val));

        // Delete the key!
        db.delete(&key);

        let retrieved = db.get(&key);
        assert_eq!(retrieved, None);
    }
    
    #[test]
    fn test_recovery_reloads_data_after_restart() {
        let db_file = "bitcask";
        let key = b"ghost".to_vec();
        let val = b"tsushima".to_vec();

        {
            let mut db = Ronin::new(db_file);
            db.put(key.clone(), val.clone());
        }

        let mut db2 = Ronin::new(db_file);

        let retrieved = db2.get(&key);
        assert_eq!(retrieved, Some(val))
    }
}
