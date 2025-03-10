use std::io::Cursor;

use arrow2::chunk::Chunk;
use arrow2::error::Error;
use arrow2::io::parquet::read::indexes;
use arrow2::{array::*, datatypes::*, error::Result, io::parquet::read::*, io::parquet::write::*};

/// Returns 2 sets of pages with different the same number of rows distributed un-evenly
fn pages(
    arrays: &[&dyn Array],
    encoding: Encoding,
) -> Result<(Vec<EncodedPage>, Vec<EncodedPage>, Schema)> {
    // create pages with different number of rows
    let array11 = PrimitiveArray::<i64>::from_slice([1, 2, 3, 4]);
    let array12 = PrimitiveArray::<i64>::from_slice([5]);
    let array13 = PrimitiveArray::<i64>::from_slice([6]);

    let schema = Schema::from(vec![
        Field::new("a1", DataType::Int64, false),
        Field::new(
            "a2",
            arrays[0].data_type().clone(),
            arrays.iter().map(|x| x.null_count()).sum::<usize>() != 0usize,
        ),
    ]);

    let parquet_schema = to_parquet_schema(&schema)?;

    let options = WriteOptions {
        write_statistics: true,
        compression: CompressionOptions::Uncompressed,
        version: Version::V1,
    };

    let pages1 = [array11, array12, array13]
        .into_iter()
        .map(|array| {
            array_to_page(
                &array,
                parquet_schema.columns()[0]
                    .descriptor
                    .primitive_type
                    .clone(),
                &[Nested::Primitive(None, true, array.len())],
                options,
                Encoding::Plain,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let pages2 = arrays
        .iter()
        .flat_map(|array| {
            array_to_pages(
                *array,
                parquet_schema.columns()[1]
                    .descriptor
                    .primitive_type
                    .clone(),
                &[Nested::Primitive(None, true, array.len())],
                options,
                encoding,
            )
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
        })
        .collect::<Vec<_>>();

    Ok((pages1, pages2, schema))
}

/// Tests reading pages while skipping indexes
fn read_with_indexes(
    (pages1, pages2, schema): (Vec<EncodedPage>, Vec<EncodedPage>, Schema),
    expected: Box<dyn Array>,
) -> Result<()> {
    let options = WriteOptions {
        write_statistics: true,
        compression: CompressionOptions::Uncompressed,
        version: Version::V1,
    };

    let to_compressed = |pages: Vec<EncodedPage>| {
        let encoded_pages = DynIter::new(pages.into_iter().map(Ok));
        let compressed_pages =
            Compressor::new(encoded_pages, options.compression, vec![]).map_err(Error::from);
        Result::Ok(DynStreamingIterator::new(compressed_pages))
    };

    let row_group = DynIter::new(vec![to_compressed(pages1), to_compressed(pages2)].into_iter());

    let writer = vec![];
    let mut writer = FileWriter::try_new(writer, schema, options)?;

    writer.write(row_group)?;
    writer.end(None)?;
    let data = writer.into_inner();

    let mut reader = Cursor::new(data);

    let metadata = read_metadata(&mut reader)?;

    let schema = infer_schema(&metadata)?;

    // row group-based filtering can be done here
    let row_groups = metadata.row_groups;

    // one per row group
    let pages = row_groups
        .iter()
        .map(|row_group| {
            assert!(indexes::has_indexes(row_group));

            indexes::read_filtered_pages(&mut reader, row_group, &schema.fields, |_, intervals| {
                let first_field = &intervals[0];
                let first_field_column = &first_field[0];
                assert_eq!(first_field_column.len(), 3);
                let selection = [false, true, false];

                first_field_column
                    .iter()
                    .zip(selection)
                    .filter_map(|(i, is_selected)| is_selected.then(|| *i))
                    .collect()
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // apply projection pushdown
    let schema = schema.filter(|index, _| index == 1);
    let pages = pages
        .into_iter()
        .map(|pages| {
            pages
                .into_iter()
                .enumerate()
                .filter(|(index, _)| *index == 1)
                .map(|(_, pages)| pages)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let expected = Chunk::new(vec![expected]);

    let chunks = FileReader::new(
        reader,
        row_groups,
        schema,
        Some(1024 * 8 * 8),
        None,
        Some(pages),
    );

    let arrays = chunks.collect::<Result<Vec<_>>>()?;

    assert_eq!(arrays, vec![expected]);
    Ok(())
}

#[test]
fn indexed_required_utf8() -> Result<()> {
    let array21 = Utf8Array::<i32>::from_slice(["a", "b", "c"]);
    let array22 = Utf8Array::<i32>::from_slice(["d", "e", "f"]);
    let expected = Utf8Array::<i32>::from_slice(["e"]).boxed();

    read_with_indexes(pages(&[&array21, &array22], Encoding::Plain)?, expected)
}

#[test]
fn indexed_required_utf8_delta() -> Result<()> {
    let array21 = Utf8Array::<i32>::from_slice(["a", "b", "c"]);
    let array22 = Utf8Array::<i32>::from_slice(["d", "e", "f"]);
    let expected = Utf8Array::<i32>::from_slice(["e"]).boxed();

    read_with_indexes(
        pages(&[&array21, &array22], Encoding::DeltaLengthByteArray)?,
        expected,
    )
}

#[test]
fn indexed_required_i32() -> Result<()> {
    let array21 = Int32Array::from_slice([1, 2, 3]);
    let array22 = Int32Array::from_slice([4, 5, 6]);
    let expected = Int32Array::from_slice([5]).boxed();

    read_with_indexes(pages(&[&array21, &array22], Encoding::Plain)?, expected)
}

#[test]
fn indexed_optional_i32() -> Result<()> {
    let array21 = Int32Array::from([Some(1), Some(2), None]);
    let array22 = Int32Array::from([None, Some(5), Some(6)]);
    let expected = Int32Array::from_slice([5]).boxed();

    read_with_indexes(pages(&[&array21, &array22], Encoding::Plain)?, expected)
}

#[test]
fn indexed_optional_utf8() -> Result<()> {
    let array21 = Utf8Array::<i32>::from([Some("a"), Some("b"), None]);
    let array22 = Utf8Array::<i32>::from([None, Some("e"), Some("f")]);
    let expected = Utf8Array::<i32>::from_slice(["e"]).boxed();

    read_with_indexes(pages(&[&array21, &array22], Encoding::Plain)?, expected)
}

#[test]
fn indexed_optional_utf8_delta() -> Result<()> {
    let array21 = Utf8Array::<i32>::from([Some("a"), Some("b"), None]);
    let array22 = Utf8Array::<i32>::from([None, Some("e"), Some("f")]);
    let expected = Utf8Array::<i32>::from_slice(["e"]).boxed();

    read_with_indexes(
        pages(&[&array21, &array22], Encoding::DeltaLengthByteArray)?,
        expected,
    )
}

#[test]
fn indexed_required_fixed_len() -> Result<()> {
    let array21 = FixedSizeBinaryArray::from_slice([[127], [128], [129]]);
    let array22 = FixedSizeBinaryArray::from_slice([[130], [131], [132]]);
    let expected = FixedSizeBinaryArray::from_slice([[131]]).boxed();

    read_with_indexes(pages(&[&array21, &array22], Encoding::Plain)?, expected)
}

#[test]
fn indexed_optional_fixed_len() -> Result<()> {
    let array21 = FixedSizeBinaryArray::from([Some([127]), Some([128]), None]);
    let array22 = FixedSizeBinaryArray::from([None, Some([131]), Some([132])]);
    let expected = FixedSizeBinaryArray::from_slice([[131]]).boxed();

    read_with_indexes(pages(&[&array21, &array22], Encoding::Plain)?, expected)
}

#[test]
fn indexed_required_boolean() -> Result<()> {
    let array21 = BooleanArray::from_slice([true, false, true]);
    let array22 = BooleanArray::from_slice([false, false, true]);
    let expected = BooleanArray::from_slice([false]).boxed();

    read_with_indexes(pages(&[&array21, &array22], Encoding::Plain)?, expected)
}

#[test]
fn indexed_optional_boolean() -> Result<()> {
    let array21 = BooleanArray::from([Some(true), Some(false), None]);
    let array22 = BooleanArray::from([None, Some(false), Some(true)]);
    let expected = BooleanArray::from_slice([false]).boxed();

    read_with_indexes(pages(&[&array21, &array22], Encoding::Plain)?, expected)
}

#[test]
fn indexed_dict() -> Result<()> {
    let indices = PrimitiveArray::from_values((0..6u64).map(|x| x % 2));
    let values = PrimitiveArray::from_slice([4i32, 6i32]).boxed();
    let array = DictionaryArray::try_from_keys(indices, values).unwrap();

    let indices = PrimitiveArray::from_slice(&[0u64]);
    let values = PrimitiveArray::from_slice([4i32, 6i32]).boxed();
    let expected = DictionaryArray::try_from_keys(indices, values).unwrap();

    let expected = expected.boxed();

    read_with_indexes(pages(&[&array], Encoding::RleDictionary)?, expected)
}
