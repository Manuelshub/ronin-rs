use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::vec;
use crc32fast::Hasher;

// Every Active file threshold.
const MAX_FILE_SIZE: u64 = 2 * 1073741824;
// struct Key(Vec<u8>);

// Format to be sent into the Bitcask file.
#[allow(dead_code)]
struct DataFormat {
    crc: u32,
    timestamp: u64,
    key_sz: u64,
    value_sz: u64,
    key: String,
    value: String,
}

// KeyDirectory 
#[allow(dead_code)]
struct KeyDirEntry {
    file_id: u32,
    value_sz: u32,
    value_pos: u64,
    timestamp: u64,
}

// After appending the DataFormat entry to the current open Bitcask file, an in-memory structure called keyDir
// is updated.
// TODO: In memory structure called keyDir
struct KeyDir(HashMap<String, KeyDirEntry>);

pub struct Ronin {
    key_dir: KeyDir,
    active_file: File,
    active_file_id: u32,
    dir_path: PathBuf,
}

// pub struct Opts {
//     pub read_only: bool,
//     pub sync_on_put: bool,
// }

// impl Default for Opts {
//     fn default() -> Self {
//         Opts {
//             read_only: false,
//             sync_on_put: false,
//         }
//     }
// }

impl Ronin {
    pub fn new<P: AsRef<Path>>(dir_path: P) -> Self {
        let dir_path_buf = dir_path.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir_path_buf).unwrap();

        let mut memory_map: HashMap<Vec<String>, KeyDirEntry> = HashMap::new();
        let mut max_file_id: u32 = 0;

        // 1. Find all data files and sort them by ID
        let mut data_files: Vec<u32> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir_path_buf) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                        if file_name.starts_with("bitcask-") && file_name.ends_with(".rn") {
                            let id_str = &file_name["bitcask-".len()..(file_name.len() - 3)];
                            if let Ok(file_id) = id_str.parse::<u32>() {
                                data_files.push(file_id);
                                if file_id > max_file_id {
                                    max_file_id = file_id;
                                }
                            }
                        }
                    }
                }
            }
        }
        
        data_files.sort_unstable(); // E.g., [0, 1, 2]
        
        // If there are no data files at all, start with ID 0
        if data_files.is_empty() {
             data_files.push(0);
        }

        // 2. Rebuild the KeyDir for each file in chronological order
        for file_id in data_files {
            let hint_path = dir_path_buf.join(format!("bitcask-{}.hint", file_id));
            let data_path = dir_path_buf.join(format!("bitcask-{}.rn", file_id));
            
            // Prefer reading from the .hint file if it exists!
            if hint_path.exists() {
                if let Ok(mut hint_file) = File::open(&hint_path) {
                    loop {
                        let mut header_buff = [0u8; 32]; // Timestamp(8) + KeySz(8) + ValSz(8) + Pos(8)
                        
                        if let Err(e) = hint_file.read_exact(&mut header_buff) {
                            if e.kind() == std::io::ErrorKind::UnexpectedEof { break; }
                            panic!("Corrupted hint file headers! {}", e);
                        }
                        
                        let timestamp = u64::from_le_bytes(header_buff[0..8].try_into().unwrap());
                        let key_sz = u64::from_le_bytes(header_buff[8..16].try_into().unwrap());
                        let value_sz = u64::from_le_bytes(header_buff[16..24].try_into().unwrap());
                        let value_pos = u64::from_le_bytes(header_buff[24..32].try_into().unwrap());
                        
                        let mut key = vec![0u8; key_sz as usize];
                        hint_file.read_exact(&mut key).unwrap();
                        
                        if value_sz == 0 {
                            memory_map.remove(&key);
                        } else {
                            let entry = KeyDirEntry { file_id, value_sz: value_sz as u32, value_pos, timestamp };
                            memory_map.insert(key, entry);
                        }
                    }
                    continue;
                }
            }
            
            
            if data_path.exists() {
               if let Ok(mut reader) = File::open(&data_path) {
                    loop {
                        let entry_pos = reader.stream_position().unwrap();
                        let mut header_buff = [0u8; 28]; // CRC(4) + Timestamp(8) + KeySz(8) + ValSz(8)

                        if let Err(e) = reader.read_exact(&mut header_buff) {
                            if e.kind() == std::io::ErrorKind::UnexpectedEof { break; }
                            panic!("Corrupted data file! Error reading headers: {}", e);
                        }

                        let timestamp = u64::from_le_bytes(header_buff[4..12].try_into().unwrap());
                        let key_sz = u64::from_le_bytes(header_buff[12..20].try_into().unwrap());
                        let value_sz = u64::from_le_bytes(header_buff[20..28].try_into().unwrap());

                        let mut key = vec![0u8; key_sz as usize];
                        reader.read_exact(&mut key).unwrap();
                        
                        reader.seek(SeekFrom::Current(value_sz as i64)).unwrap();

                        if value_sz == 0 {
                            memory_map.remove(&key);
                        } else {
                            let entry = KeyDirEntry { file_id, value_sz: value_sz as u32, value_pos: entry_pos, timestamp };
                            memory_map.insert(key, entry);
                        }
                    }
               }
            }
        }

        let active_file_path = dir_path_buf.join(format!("bitcask-{}.rn", max_file_id));
        let active_file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(active_file_path)
            .expect("Failed to open or create the active database file");

        let key_dir = KeyDir(memory_map);

        Ronin {
            key_dir,
            active_file,
            active_file_id: max_file_id,
            dir_path: dir_path_buf,
        }
    }

    // Returns true if the filesize is more than or equal to maximum filesize and false if otherwise 
    fn reach_file_threshold(&mut self) -> bool {
        let current_pos = self.active_file.stream_position().unwrap();

        if current_pos  < MAX_FILE_SIZE {
            false
        } else {
            true
        }
    }

    fn rotate_active_file(&mut self) -> u64 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
            
        self.active_file_id += 1;

        let data_file_path = self.dir_path.join(format!("{}_{}.rn", timestamp, self.active_file_id));
        let new_active_file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(data_file_path)
            .unwrap();

        self.active_file = new_active_file;

        self.active_file.stream_position().unwrap()
    }

    pub fn open<P: AsRef<Path>>(&mut self, dir_name: P) -> Self {
        // Calling new for now, since it mimics open logic.
        Ronin::new(dir_name)
    }

    pub fn write(&mut self, key: &str, value: &str) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let buffer_sz = 4 + 8 + 8 + 8 + key.len() + value.len();
        let mut entry_buffer: Vec<u8> = Vec::with_capacity(buffer_sz);

        let timestamp_bytes = timestamp.to_le_bytes();
        let key_sz_bytes = key.len().to_le_bytes();
        let val_sz_bytes = value.len().to_le_bytes();
        
        entry_buffer.extend_from_slice(&[0; 4]);
        entry_buffer.extend_from_slice(&timestamp_bytes);
        entry_buffer.extend_from_slice(&key_sz_bytes);
        entry_buffer.extend_from_slice(&val_sz_bytes);
        entry_buffer.extend_from_slice(key.as_bytes());
        entry_buffer.extend(value.as_bytes());

        let mut hasher = Hasher::new();
        hasher.update(&entry_buffer[4..]);

        let checksum = hasher.finalize();
        entry_buffer[0..4].copy_from_slice(&checksum.to_le_bytes());

        let mut current_pos = self.active_file.stream_position().unwrap();

        if self.reach_file_threshold() {
            current_pos = self.rotate_active_file();
        }

        self.active_file.write_all(&entry_buffer).unwrap();

        let entry = KeyDirEntry{
            file_id: self.active_file_id,
            value_sz: value.len() as u32,
            value_pos: current_pos,
            timestamp: timestamp,
        };

        self.key_dir.0.insert(key.to_string(), entry);
    }

    pub fn put(&mut self, key: &str, value: &str) {
        self.write(key, value);
    }

    pub fn get(&mut self, key: &str) -> Option<String> {
        let entry = self.key_dir.0.get(key)?;
        let val_position = entry.value_pos;
        let val_size = entry.value_sz as usize;
        let val_offset = val_position + 4 + 8 + 8 + 8 + (key.len() as u64);

        let mut val_buffer= String::with_capacity(val_size);
        let bytes_buffer = unsafe {
            val_buffer.as_bytes_mut()
        };
        
        if entry.file_id == self.active_file_id {
            self.active_file.seek(SeekFrom::Start(val_offset)).unwrap();
            self.active_file.read_exact(bytes_buffer).unwrap();
        } else {
            let old_file_path = self.dir_path.join(format!("{}_{}.rn", entry.timestamp, entry.file_id));

            let mut old_file = File::open(old_file_path).unwrap();
            old_file.seek(SeekFrom::Start(val_offset)).unwrap();
            old_file.read_exact(bytes_buffer).unwrap();
        }

        Some(val_buffer)
    }

    pub fn delete(&mut self, key: &str) {
        let tbs_value = "GONE";
        self.put(key, tbs_value);
        self.key_dir.0.remove(key);
    }

    pub fn list_keys(&mut self) -> Vec<String> {
        self.key_dir.0.keys().cloned().collect()
    }

    pub fn merge(&mut self) {
        let keys: Vec<String> = self.key_dir.0.keys().cloned().collect();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    
        let merge_file_id = self.active_file_id + 1;
        let merge_file_path = self.dir_path.join(format!("{}_{}.rn", timestamp, merge_file_id));
        let mut merge_file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .read(true)
            .open(merge_file_path)
            .unwrap();
        
        let hint_file_path = self.dir_path.join(format!("{}_{}.hint", timestamp, merge_file_id));
        let mut hint_file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .read(true)
            .open(hint_file_path)
            .unwrap();

        let mut new_key_dir = HashMap::new();
        let mut current_pos: u64 = 0;

        for key in keys {
            if let Some(val) = self.get(&key) {
                let key_sz = key.len() as u64;
                let value_sz = val.len() as u64;
                let buffer_sz = 4 + 8 + 8 + 8 + key_sz + value_sz;
                let hint_buffer_sz = 8 + 8 + 8 + 8 + key.len(); //hint files buffer size
                let mut entry_buffer = Vec::with_capacity(buffer_sz as usize);
                let mut hint_buffer = Vec::with_capacity(hint_buffer_sz);

                // Constructing the buffer for our merge file.
                entry_buffer.extend_from_slice(&[0; 4]);
                entry_buffer.extend_from_slice(&timestamp.to_le_bytes());
                entry_buffer.extend_from_slice(&key_sz.to_le_bytes());
                entry_buffer.extend_from_slice(&value_sz.to_le_bytes());
                entry_buffer.extend_from_slice(&key.as_bytes());
                entry_buffer.extend(&mut val.as_bytes().iter());

                // Constructing the buffer for our hint file.
                hint_buffer.extend_from_slice(&timestamp.to_le_bytes());
                hint_buffer.extend_from_slice(&key_sz.to_le_bytes());
                hint_buffer.extend_from_slice(&value_sz.to_le_bytes());
                hint_buffer.extend_from_slice(&current_pos.to_le_bytes());
                hint_buffer.extend_from_slice(&key.as_bytes());

                hint_file.write_all(&hint_buffer).unwrap();

                let mut hasher = Hasher::new();
                hasher.update(&entry_buffer[4..]);

                let checksum = hasher.finalize();
                entry_buffer[0..4].copy_from_slice(&checksum.to_le_bytes());

                // current_pos = merge_file.stream_position().unwrap();

                merge_file.write_all(&entry_buffer).unwrap();

                let entry = KeyDirEntry {
                    file_id: merge_file_id,
                    value_sz: value_sz as u32,
                    value_pos: current_pos,
                    timestamp: timestamp,
                };
                new_key_dir.insert(key.clone(), entry);
                current_pos += entry_buffer.len() as u64;
            }
        }
        // After the merge operation, we update our Ronin structure with the current merge file, merge_file_id and the new_key_dir structure.
        self.active_file = merge_file;
        self.active_file_id = merge_file_id;
        self.key_dir.0 = new_key_dir;

        let ronin_dir = fs::read_dir(&self.dir_path).unwrap();
        
        // We loop through the ronin directory to get each directory entry.
        for dir_entry in ronin_dir {
            let file_path = dir_entry.unwrap().path();

            if let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) {
                // Check if it's a ronin file
                if file_name.ends_with(".rn") {
                    let timestamp_string = timestamp.to_string();
                    let timestamp_on_file = format!("{}_", &timestamp_string);
                    let id_str = &file_name[timestamp_on_file.len()..(file_name.len() -3)];
                    if let Ok(file_id) = id_str.parse::<u32>() {
                        if file_id < merge_file_id {
                            fs::remove_file(file_path).unwrap();
                        }
                    }
                }
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_put_retrieves_correct_data() {
        let dir = tempdir().unwrap();
        let mut db = Ronin::new(dir.path());

        let key1 = "kenechukwu";
        let val1 = "ifediora";
        db.put(key1, val1);

        let retrieved = db.get(key1);

        assert_eq!(retrieved, Some(val1));
    }

    #[test]
    fn test_delete_removes_key() {
        let dir = tempdir().unwrap();
        let mut db = Ronin::new(dir.path());

        let key = "Shinske";
        let val = "Nakamura";

        db.put(key.clone(), val);
        assert_eq!(db.get(key), Some(val));

        // Delete the key!
        db.delete(key);

        let retrieved = db.get(key);
        assert_eq!(retrieved, None);
    }
    
    #[test]
    fn test_recovery_reloads_data_after_restart() {
        let dir = tempdir().unwrap();
        let db_file = dir.path();
        let key = "ghost";
        let val = "tsushima";

        {
            let mut db = Ronin::new(db_file);
            db.put(key, val);
        }

        let mut db2 = Ronin::new(db_file);

        let retrieved = db2.get(key);
        assert_eq!(retrieved, Some(val));
    }

    #[test]
    fn test_merge_compacts_data_files() {
        let dir = tempdir().unwrap();
        let db_file = dir.path();

        let key = "A";
        let val1 = "foo";
        let val2 = "bar";
        let val3 = "baz";

        let mut db = Ronin::new(db_file);
        
        // Write the same key multiple times to create stale records
        db.put(key.clone(), val1);
        db.put(key.clone(), val2);
        db.put(key, val3); // "baz" is the freshest data

        // Perform the merge
        db.merge();

        // Verify the latest value is intact
        let retrieved = db.get(key);
        assert_eq!(retrieved, Some(val3));

        // Verify there is only one .rn file left after cleanup
        let mut rn_count = 0;
        if let Ok(entries) = std::fs::read_dir(db_file) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("rn") {
                    rn_count += 1;
                }
            }
        }
        
        assert_eq!(rn_count, 1, "Merge should leave exactly 1 .rn file");
    }

    #[test]
    fn test_merge_put_instance() {
        let dir = tempdir().unwrap();
        let mut db = Ronin::new(dir.path());
        let mut idx = 0;
        let key = "always_ronin";
        
        while idx < 100 {
            let format = format!("Data-{}", idx);
            let val = format.as_str();

            db.put(key, val);
            idx += 1;
        }
        
        let last_val = db.get(&key);
        assert_eq!(last_val, Some("Data-99"));
    }
}
