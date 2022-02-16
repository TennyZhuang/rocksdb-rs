use crate::common::{FileSystem, InternalKeyComparator};
use crate::table::{BlockBasedTableFactory, TableFactory};
use crate::{KeyComparator, SyncPoxisFileSystem};
use std::sync::Arc;

pub struct ImmutableDBOptions {
    pub max_manifest_file_size: usize,
    pub db_path: String,
    pub fs: Arc<dyn FileSystem>,
    pub max_background_jobs: usize,
}

#[derive(Clone)]
pub struct ColumnFamilyOptions {
    pub write_buffer_size: usize,
    pub max_write_buffer_number: usize,
    pub factory: Arc<dyn TableFactory>,
    pub comparator: InternalKeyComparator,
}

impl PartialEq for ColumnFamilyOptions {
    fn eq(&self, other: &Self) -> bool {
        self.write_buffer_size == other.write_buffer_size
            && self.max_write_buffer_number == other.max_write_buffer_number
            && self.factory.name().eq(other.factory.name())
            && self.comparator.name().eq(other.comparator.name())
    }
}

impl Eq for ColumnFamilyOptions {}

impl Default for ColumnFamilyOptions {
    fn default() -> Self {
        ColumnFamilyOptions {
            write_buffer_size: 4 << 20,
            max_write_buffer_number: 1,
            factory: Arc::new(BlockBasedTableFactory::default()),
            comparator: InternalKeyComparator::default(),
        }
    }
}

#[derive(Clone)]
pub struct DBOptions {
    pub max_manifest_file_size: usize,
    pub create_if_missing: bool,
    pub create_missing_column_families: bool,
    pub fs: Arc<dyn FileSystem>,
    pub db_path: String,
    pub db_name: String,
    pub max_background_jobs: usize,
}

impl Default for DBOptions {
    fn default() -> Self {
        Self {
            max_manifest_file_size: 0,
            create_if_missing: false,
            create_missing_column_families: false,
            fs: Arc::new(SyncPoxisFileSystem {}),
            db_path: "db".to_string(),
            db_name: "db".to_string(),
            max_background_jobs: 0,
        }
    }
}

pub struct ColumnFamilyDescriptor {
    pub name: String,
    pub options: ColumnFamilyOptions,
}

impl From<DBOptions> for ImmutableDBOptions {
    fn from(opt: DBOptions) -> Self {
        Self {
            max_manifest_file_size: opt.max_manifest_file_size,
            db_path: "".to_string(),
            fs: opt.fs.clone(),
            max_background_jobs: opt.max_background_jobs,
        }
    }
}
