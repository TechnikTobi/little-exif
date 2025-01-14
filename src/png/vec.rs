// Copyright © 2024 Tobias J. Prisching <tobias.prisching@icloud.com> and CONTRIBUTORS
// See https://github.com/TechnikTobi/little_exif#license for licensing details

use std::io::Cursor;
use std::io::Read;
use std::io::Seek;

use crc::Crc;
use crc::CRC_32_ISO_HDLC;
use miniz_oxide::deflate::compress_to_vec_zlib;
use miniz_oxide::inflate::decompress_to_vec_zlib;

use crate::general_file_io::*;
use crate::metadata::Metadata;
use crate::util::insert_multiple_at;
use crate::util::range_remove;

use super::PNG_SIGNATURE;
use super::RAW_PROFILE_TYPE_EXIF;

use super::png_chunk::PngChunk;
use super::decode_metadata_png;
use super::encode_metadata_png;

fn
check_signature
(
	file_buffer: &Vec<u8>
)
-> Result<Cursor<&Vec<u8>>, std::io::Error>
{	
	// Check the signature
	let signature_is_valid = file_buffer[0..8].iter()
		.zip(PNG_SIGNATURE.iter())
		.filter(|&(read, constant)| read == constant)
		.count() == PNG_SIGNATURE.len();

	if !signature_is_valid
	{
		return io_error!(InvalidData, "Can't open PNG file - Wrong signature!");
	}

	// Signature is valid - can proceed using the data as PNG file
	let mut cursor = Cursor::new(file_buffer);
	cursor.set_position(8);
	return Ok(cursor);
}

// TODO: Check if this is also affected by endianness
// Edit: Should... not? I guess?
fn
get_next_chunk_descriptor
(
	cursor: &mut Cursor<&Vec<u8>>
)
-> Result<PngChunk, std::io::Error>
{
	// Read the start of the chunk
	let mut chunk_start = [0u8; 8];
	let mut bytes_read = cursor.read(&mut chunk_start).unwrap();

	// Check that indeed 8 bytes were read
	if bytes_read != 8
	{
		return io_error!(Other, "Could not read start of chunk");
	}

	// Construct name of chunk and its length
	let chunk_name = String::from_utf8((&chunk_start[4..8]).to_vec());
	let mut chunk_length = 0u32;
	for byte in &chunk_start[0..4]
	{
		chunk_length = chunk_length * 256 + *byte as u32;
	}

	// Read chunk data ...
	let mut chunk_data_buffer = vec![0u8; chunk_length as usize];
	bytes_read = cursor.read(&mut chunk_data_buffer).unwrap();
	if bytes_read != chunk_length as usize
	{
		return io_error!(Other, "Could not read chunk data");
	}

	// ... and CRC values
	let mut chunk_crc_buffer = [0u8; 4];
	bytes_read = cursor.read(&mut chunk_crc_buffer).unwrap();
	if bytes_read != 4
	{
		return io_error!(Other, "Could not read chunk CRC");
	}

	// Compute CRC on chunk
	let mut crc_input = Vec::new();
	crc_input.extend(chunk_start[4..8].iter());
	crc_input.extend(chunk_data_buffer.iter());

	let crc_struct = Crc::<u32>::new(&CRC_32_ISO_HDLC);
	let checksum = crc_struct.checksum(&crc_input) as u32;

	for i in 0..4
	{
		if ((checksum >> (8 * (3-i))) as u8) != chunk_crc_buffer[i]
		{
			return io_error!(InvalidData, "Checksum check failed while reading PNG!");
		}
	}

	// If validating the chunk using the CRC was successful, return its descriptor
	// Note: chunk_length does NOT include the +4 for the CRC area!
	if let Ok(png_chunk) = PngChunk::from_string(
		&chunk_name.unwrap(),
		chunk_length
	)
	{
		return Ok(png_chunk);
	}
	else
	{
		return io_error!(Other, "Invalid PNG chunk name");
	}
}

/// "Parses" the PNG by checking various properties:
/// - Can the file be opened and is the signature valid?
/// - Are the various chunks OK or not? For this, the local subroutine `get_next_chunk_descriptor` is used
pub(crate) fn
parse_png
(
	file_buffer: &Vec<u8>
)
-> Result<Vec<PngChunk>, std::io::Error>
{
	let mut cursor = check_signature(file_buffer)?;
	let mut chunks = Vec::new();

	loop
	{
		let chunk_descriptor = get_next_chunk_descriptor(&mut cursor)?;
		chunks.push(chunk_descriptor);

		if chunks.last().unwrap().as_string() == "IEND".to_string()
		{
			break;
		}
	}

	return Ok(chunks);
}

// Clears existing metadata chunk from a png file
// Gets called before writing any new metadata
#[allow(non_snake_case)]
pub(crate) fn
clear_metadata
(
	file_buffer: &mut Vec<u8>
)
-> Result<(), std::io::Error>
{

	// Parse the PNG - if this fails, the clear operation fails as well
	let parse_png_result = parse_png(&file_buffer)?;

	// Parsed PNG is Ok to use - Open the file and go through the chunks
	// let mut file = open_write_file(path)?;
	let mut cursor = Cursor::new(file_buffer);
	let mut seek_counter = 8u64;

	for chunk in &parse_png_result
	{
		// If this is not a zTXt chunk, jump to the next chunk
		if chunk.as_string() != String::from("zTXt")
		{
			seek_counter += chunk.length() as u64 + 12;
			cursor.seek(std::io::SeekFrom::Current(chunk.length() as i64 + 12))?;
			continue;
		}

		// Skip chunk length and type (4+4 Bytes)
		cursor.seek(std::io::SeekFrom::Current(4+4))?;

		// Read chunk data into buffer for checking that this is the 
		// correct chunk to delete
		let mut zTXt_chunk_data = vec![0u8; chunk.length() as usize];

		if cursor.read(&mut zTXt_chunk_data).unwrap() != chunk.length() as usize
		{
			return io_error!(Other, "Could not read chunk data");
		}

		// Compare to the "Raw profile type exif" string constant
		let mut correct_zTXt_chunk = true;
		for i in 0..RAW_PROFILE_TYPE_EXIF.len()
		{
			if zTXt_chunk_data[i] != RAW_PROFILE_TYPE_EXIF[i]
			{
				correct_zTXt_chunk = false;
				break;
			}
		}

		// Skip the CRC as it is not important at this point
		cursor.seek(std::io::SeekFrom::Current(4))?;

		// If this is not the correct zTXt chunk, ignore current
		// (wrong) zTXt chunk and continue with next chunk
		if !correct_zTXt_chunk
		{	
			continue;
		}
		
		// We have now established that this is the correct chunk to delete
		let remove_start = seek_counter as usize;
		let remove_end   = cursor.position() as usize;
		range_remove(cursor.get_mut(), remove_start, remove_end);
	}

	return Ok(());
}

#[allow(non_snake_case)]
pub(crate) fn
read_metadata
(
	file_buffer: &Vec<u8>
)
-> Result<Vec<u8>, std::io::Error>
{
	// Parse the PNG - if this fails, the read fails as well
	let parse_png_result = parse_png(file_buffer)?;

	// Parsed PNG is Ok to use - Open the file and go through the chunks
	let mut cursor = check_signature(file_buffer).unwrap();
	for chunk in &parse_png_result
	{
		// Wrong chunk? Seek to the next one
		if chunk.as_string() != String::from("zTXt")
		{
			cursor.seek(std::io::SeekFrom::Current(chunk.length() as i64 + 12))?;
			continue;
		}

		// We now have a zTXt chunk:
		// Skip chunk length and type (4+4 Bytes)
		cursor.seek(std::io::SeekFrom::Current(4+4))?;

		// Read chunk data into buffer
		// No need to verify this using CRC as already done by parse_png(path)
		let mut zTXt_chunk_data = vec![0u8; chunk.length() as usize];
		if cursor.read(&mut zTXt_chunk_data).unwrap() != chunk.length() as usize
		{
			return io_error!(Other, "Could not read chunk data");
		}

		// Check that this is the correct zTXt chunk...
		let mut correct_zTXt_chunk = true;
		for i in 0..RAW_PROFILE_TYPE_EXIF.len()
		{
			if zTXt_chunk_data[i] != RAW_PROFILE_TYPE_EXIF[i]
			{
				correct_zTXt_chunk = false;
				break;
			}
		}

		if !correct_zTXt_chunk
		{
			// Skip CRC from current (wrong) zTXt chunk and continue
			cursor.seek(std::io::SeekFrom::Current(4))?;
			continue;
		}

		// Decode zlib data...
		if let Ok(decompressed_data) = decompress_to_vec_zlib(&zTXt_chunk_data[RAW_PROFILE_TYPE_EXIF.len()..])
		{
			// ...and perform PNG-specific decoding & return the result
			return Ok(decode_metadata_png(&decompressed_data).unwrap());
		}
		else
		{
			return io_error!(Other, "Could not inflate compressed chunk data!");
		}
	}

	return io_error!(Other, "No metadata found!");

}



#[allow(non_snake_case)]
pub(crate) fn
write_metadata
(
	file_buffer: &mut Vec<u8>,
	metadata:    &Metadata
)
-> Result<(), std::io::Error>
{
	// First clear the existing metadata
	// This also parses the PNG and checks its validity, so it is safe to
	// assume that is, in fact, a usable PNG file
	let _ = clear_metadata(file_buffer)?;

	let mut IHDR_length = 0u32;
	if let Ok(chunks) = parse_png(file_buffer)
	{
		IHDR_length = chunks[0].length();
	}

	// Encode the data specifically for PNG and open the image file
	let encoded_metadata = encode_metadata_png(&metadata.encode()?);
	let seek_start = 0u64         // Skip ...
	+ PNG_SIGNATURE.len() as u64  // PNG Signature
	+ IHDR_length         as u64  // IHDR data section
	+ 12                  as u64; // rest of IHDR chunk (length, type, CRC)

	// Build data of new chunk using zlib compression (level=8 -> default)
	let mut zTXt_chunk_data: Vec<u8> = vec![0x7a, 0x54, 0x58, 0x74];
	zTXt_chunk_data.extend(RAW_PROFILE_TYPE_EXIF.iter());
	zTXt_chunk_data.extend(compress_to_vec_zlib(&encoded_metadata, 8).iter());

	// Compute CRC and append it to the chunk data
	let crc_struct = Crc::<u32>::new(&CRC_32_ISO_HDLC);
	let checksum = crc_struct.checksum(&zTXt_chunk_data) as u32;
	for i in 0..4
	{
		zTXt_chunk_data.push( (checksum >> (8 * (3-i))) as u8);		
	}

	// Prepare the length of the new chunk (subtracting 8 for type and CRC) for
	// inserting prior to the new chunk
	let     chunk_data_len        = zTXt_chunk_data.len() as u32 - 8;
	let mut chunk_data_len_buffer = [0u8; 4];
	for i in 0..4
	{
		chunk_data_len_buffer[i] = (chunk_data_len >> (8 * (3-i))) as u8;
	}
	
	// Write data of new chunk length and chunk itself
	let insert_position = seek_start as usize;
	insert_multiple_at(file_buffer, insert_position,   &mut chunk_data_len_buffer.to_vec());
	insert_multiple_at(file_buffer, insert_position+4, &mut zTXt_chunk_data);

	return Ok(());
}

#[cfg(test)]
mod tests 
{

	#[test]
	fn
	parsing_test() 
	{
		let chunks = crate::png::file::parse_png(
			std::path::Path::new("tests/png_parse_test_image.png")
		).unwrap();
		assert_eq!(chunks.len(), 3);
	}
	
}
