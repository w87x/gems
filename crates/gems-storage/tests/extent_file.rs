use gems_storage::extent::{
    Bitmap, ExtentHeader, BITMAP_BYTES, DATA_OFFSET, EXTENT_HEADER_LEN, EXTENT_SIZE,
};
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
        // `len()` is the mmap's OS-page-rounded length, not the logical
        // EXTENT_SIZE (which isn't itself page-aligned) — it only needs to
        // cover at least one extent.
        assert!(file.len() as u64 >= EXTENT_SIZE);

        let class = BlockClass::from_block_size(512).unwrap();
        let header = ExtentHeader::new(class);

        let slice = file.as_mut_slice().unwrap();
        slice[0..4].copy_from_slice(&header.magic.to_le_bytes());
        slice[4..6].copy_from_slice(&header.format_version.to_le_bytes());
        slice[6] = header.block_class;

        {
            let mut bm =
                Bitmap::new(&mut slice[EXTENT_HEADER_LEN..EXTENT_HEADER_LEN + BITMAP_BYTES]);
            let a = bm.allocate(class.slots_per_extent()).unwrap();
            assert_eq!(a, 0);
        }
        // write a byte into the first data slot
        slice[DATA_OFFSET] = 0xAB;
        file.sync_range(0, DATA_OFFSET + 1).unwrap();
    }

    {
        let file = ExtentFile::open(&path, false).unwrap();
        assert!(file.len() as u64 >= EXTENT_SIZE);
        let slice = file.as_slice();
        assert_eq!(
            u32::from_le_bytes(slice[0..4].try_into().unwrap()),
            gems_storage::extent::EXTENT_MAGIC
        );
        let mut bitmap_copy = slice[EXTENT_HEADER_LEN..EXTENT_HEADER_LEN + BITMAP_BYTES].to_vec();
        let bm = Bitmap::new(&mut bitmap_copy);
        assert!(bm.is_allocated(0));
        assert_eq!(slice[DATA_OFFSET], 0xAB);
    }

    std::fs::remove_file(&path).ok();
}
