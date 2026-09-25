use gems_storage::extent::{Bitmap, ExtentHeader, BITMAP_BYTES, EXTENT_SIZE, HEADER_FIELDS_SIZE};
use gems_storage::file::ExtentFile;
use gems_storage::BlockClass;

#[test]
fn grow_write_reopen_roundtrip() {
    let dir = std::env::temp_dir().join(format!("gems-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("extent_file_roundtrip.gems");
    let _ = std::fs::remove_file(&path);

    {
        let mut file = ExtentFile::create(&path).unwrap();
        let offset = file.grow_by_one_extent().unwrap();
        assert_eq!(offset, 0);
        assert_eq!(file.len() as u64, EXTENT_SIZE);

        let class = BlockClass::from_block_size(512).unwrap();
        let header = ExtentHeader::new(class);

        let slice = file.as_mut_slice().unwrap();
        slice[0..4].copy_from_slice(&header.magic.to_le_bytes());
        slice[4..6].copy_from_slice(&header.format_version.to_le_bytes());
        slice[6] = header.block_class;

        {
            let mut bm = Bitmap::new(&mut slice[HEADER_FIELDS_SIZE..BITMAP_BYTES]);
            let a = bm.allocate(class.slots_per_extent()).unwrap();
            assert_eq!(a, 0);
        }
        // write a byte into the first data slot
        slice[BITMAP_BYTES] = 0xAB;
        file.sync_range(0, BITMAP_BYTES + 1).unwrap();
    }

    {
        let file = ExtentFile::open(&path, false).unwrap();
        assert_eq!(file.len() as u64, EXTENT_SIZE);
        let slice = file.as_slice();
        assert_eq!(
            u32::from_le_bytes(slice[0..4].try_into().unwrap()),
            gems_storage::extent::EXTENT_MAGIC
        );
        let mut bitmap_copy = slice[HEADER_FIELDS_SIZE..BITMAP_BYTES].to_vec();
        let bm = Bitmap::new(&mut bitmap_copy);
        assert!(bm.is_allocated(0));
        assert_eq!(slice[BITMAP_BYTES], 0xAB);
    }

    std::fs::remove_file(&path).ok();
}
