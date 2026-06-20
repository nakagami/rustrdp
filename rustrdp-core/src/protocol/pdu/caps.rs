use crate::core::io::*;

pub const CAPSTYPE_GENERAL: u16 = 0x0001;
pub const CAPSTYPE_BITMAP: u16 = 0x0002;
pub const CAPSTYPE_ORDER: u16 = 0x0003;
pub const CAPSTYPE_BITMAPCACHE: u16 = 0x0004;
pub const CAPSTYPE_CONTROL: u16 = 0x0005;
pub const CAPSTYPE_ACTIVATION: u16 = 0x0007;
pub const CAPSTYPE_POINTER: u16 = 0x0008;
pub const CAPSTYPE_SHARE: u16 = 0x0009;
pub const CAPSTYPE_COLORCACHE: u16 = 0x000A;
pub const CAPSTYPE_SOUND: u16 = 0x000C;
pub const CAPSTYPE_INPUT: u16 = 0x000D;
pub const CAPSTYPE_FONT: u16 = 0x000E;
pub const CAPSTYPE_BRUSH: u16 = 0x000F;
pub const CAPSTYPE_GLYPHCACHE: u16 = 0x0010;
pub const CAPSTYPE_OFFSCREENCACHE: u16 = 0x0011;
pub const CAPSTYPE_BITMAPCACHE_HOSTSUPPORT: u16 = 0x0012;
pub const CAPSTYPE_BITMAPCACHE_REV2: u16 = 0x0013;
pub const CAPSTYPE_VIRTUAL_CHANNEL: u16 = 0x0014;
pub const CAPSTYPE_RAIL: u16 = 0x0017;
pub const CAPSETTYPE_COMPDESK: u16 = 0x0019;
pub const CAPSETTYPE_MULTIFRAGMENTUPDATE: u16 = 0x001A;
pub const CAPSETTYPE_LARGE_POINTER: u16 = 0x001B;
pub const CAPSETTYPE_SURFACE_COMMANDS: u16 = 0x001C;
pub const CAPSETTYPE_BITMAP_CODECS: u16 = 0x001D;
pub const CAPSSETTYPE_FRAME_ACKNOWLEDGE: u16 = 0x001E;

fn build_capability(cap_type: u16, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u16_le(&mut buf, cap_type);
    write_u16_le(&mut buf, (data.len() + 4) as u16);
    buf.extend_from_slice(data);
    buf
}

// CAPSTYPE_GENERAL (0x0001): 26 bytes
fn build_general_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 1); // osMajorType
    write_u16_le(&mut inner, 3); // osMinorType
    write_u16_le(&mut inner, 0x0200); // protocolVersion
    write_u16_le(&mut inner, 0); // pad2octetsA
    write_u16_le(&mut inner, 0); // generalCompressionTypes
    write_u16_le(&mut inner, 0x040D); // extraFlags (FastPath+LongCreds+AutoReconnect+NoBmpCompressHdr)
    write_u16_le(&mut inner, 0); // updateCapabilityFlag
    write_u16_le(&mut inner, 0); // remoteUnshareFlag
    write_u16_le(&mut inner, 0); // generalCompressionLevel
    write_u8(&mut inner, 1); // refreshRectSupport
    write_u8(&mut inner, 1); // suppressOutputSupport
    build_capability(CAPSTYPE_GENERAL, &inner)
}

// CAPSTYPE_BITMAP (0x0002): 30 bytes
fn build_bitmap_capability(width: u16, height: u16) -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 32); // preferredBitsPerPixel
    write_u16_le(&mut inner, 1); // receive1BitPerPixel
    write_u16_le(&mut inner, 1); // receive4BitsPerPixel
    write_u16_le(&mut inner, 1); // receive8BitsPerPixel
    write_u16_le(&mut inner, width);
    write_u16_le(&mut inner, height);
    write_u16_le(&mut inner, 0); // pad2octets
    write_u16_le(&mut inner, 1); // desktopResizeFlag
    write_u16_le(&mut inner, 1); // bitmapCompressionFlag
    write_u8(&mut inner, 0); // highColorFlags
    write_u8(&mut inner, 0); // drawingFlags
    write_u16_le(&mut inner, 1); // multipleRectangleSupport
    write_u16_le(&mut inner, 0); // pad2octetsB
    build_capability(CAPSTYPE_BITMAP, &inner)
}

// CAPSTYPE_ORDER (0x0003): 92 bytes (88 inner)
fn build_order_capability() -> Vec<u8> {
    let mut inner = vec![0u8; 16]; // terminalDescriptor[16]
    write_u32_le(&mut inner, 0); // pad4octetsA
    write_u16_le(&mut inner, 1); // desktopSaveXGranularity
    write_u16_le(&mut inner, 20); // desktopSaveYGranularity
    write_u16_le(&mut inner, 0); // pad2octetsA
    write_u16_le(&mut inner, 1); // maximumOrderLevel
    write_u16_le(&mut inner, 0); // numberFonts
    write_u16_le(&mut inner, 0x00AA); // orderFlags
                                      // orderSupport[32]: DstBlt, PatBlt, ScrBlt supported
    let mut order_support = [0u8; 32];
    order_support[0] = 1; // DstBlt
    order_support[1] = 1; // PatBlt
    order_support[2] = 1; // ScrBlt
    inner.extend_from_slice(&order_support);
    write_u16_le(&mut inner, 0); // textFlags
    write_u16_le(&mut inner, 4); // orderSupportExFlags
    write_u32_le(&mut inner, 0); // pad4octetsB
    write_u32_le(&mut inner, 480 * 480); // desktopSaveSize
    write_u16_le(&mut inner, 0); // pad2octetsC
    write_u16_le(&mut inner, 0); // pad2octetsD
    write_u16_le(&mut inner, 1252); // textANSICodePage
    write_u16_le(&mut inner, 0); // pad2octetsE
    build_capability(CAPSTYPE_ORDER, &inner)
}

// CAPSTYPE_BITMAPCACHE_REV2 (0x0013): 40 bytes
fn build_bitmapcache_rev2_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 2); // cacheFlags (ALLOW_CACHE_WAITING_LIST_FLAG)
    write_u8(&mut inner, 0); // pad2
    write_u8(&mut inner, 5); // numCellCaches
    write_u32_le(&mut inner, 600); // bitmapCache0CellInfo
    write_u32_le(&mut inner, 600); // bitmapCache1CellInfo
    write_u32_le(&mut inner, 2048); // bitmapCache2CellInfo
    write_u32_le(&mut inner, 4096); // bitmapCache3CellInfo
    write_u32_le(&mut inner, 2048); // bitmapCache4CellInfo
    inner.extend_from_slice(&[0u8; 12]); // pad3
    build_capability(CAPSTYPE_BITMAPCACHE_REV2, &inner)
}

// CAPSTYPE_CONTROL (0x0005): 12 bytes
fn build_control_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 0); // controlFlags
    write_u16_le(&mut inner, 0); // remoteDetachFlag
    write_u16_le(&mut inner, 2); // controlInterest
    write_u16_le(&mut inner, 2); // detachInterest
    build_capability(CAPSTYPE_CONTROL, &inner)
}

// CAPSTYPE_ACTIVATION (0x0007): 12 bytes
fn build_activation_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 0); // helpKeyFlag
    write_u16_le(&mut inner, 0); // helpKeyIndexFlag
    write_u16_le(&mut inner, 0); // helpExtendedKeyFlag
    write_u16_le(&mut inner, 0); // windowManagerKeyFlag
    build_capability(CAPSTYPE_ACTIVATION, &inner)
}

// CAPSTYPE_POINTER (0x0008): 10 bytes
fn build_pointer_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 1); // colorPointerFlag
    write_u16_le(&mut inner, 20); // colorPointerCacheSize
    write_u16_le(&mut inner, 20); // pointerCacheSize
    build_capability(CAPSTYPE_POINTER, &inner)
}

// CAPSTYPE_SHARE (0x0009): 8 bytes
fn build_share_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 0); // nodeId
    write_u16_le(&mut inner, 0); // pad2octets
    build_capability(CAPSTYPE_SHARE, &inner)
}

// CAPSTYPE_COLORCACHE (0x000A): 8 bytes
fn build_colorcache_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 6); // cacheSize
    write_u16_le(&mut inner, 0); // pad2octets
    build_capability(CAPSTYPE_COLORCACHE, &inner)
}

// CAPSTYPE_SOUND (0x000C): 8 bytes
fn build_sound_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 1); // flags (SOUND_BEEPS_FLAG)
    write_u16_le(&mut inner, 0); // pad2octets
    build_capability(CAPSTYPE_SOUND, &inner)
}

// CAPSTYPE_INPUT (0x000D): 88 bytes
fn build_input_capability(kbd_layout: u32) -> Vec<u8> {
    let mut inner = Vec::new();
    // INPUT_FLAG_SCANCODES|MOUSEX|FASTPATH_INPUT|UNICODE|FASTPATH_INPUT2
    write_u16_le(&mut inner, 0x003D);
    write_u16_le(&mut inner, 0);
    write_u32_le(&mut inner, kbd_layout);
    write_u32_le(&mut inner, 4); // keyboardType (IBM enhanced)
    write_u32_le(&mut inner, 0); // keyboardSubType
    write_u32_le(&mut inner, 12); // keyboardFunctionKey
    inner.extend_from_slice(&[0u8; 64]); // imeFileName
    build_capability(CAPSTYPE_INPUT, &inner)
}

// CAPSTYPE_FONT (0x000E): 8 bytes
fn build_font_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 1); // fontSupportFlags
    write_u16_le(&mut inner, 0); // pad2octets
    build_capability(CAPSTYPE_FONT, &inner)
}

// CAPSTYPE_BRUSH (0x000F): 8 bytes
fn build_brush_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u32_le(&mut inner, 1); // brushSupportLevel (COLOR_8x8)
    build_capability(CAPSTYPE_BRUSH, &inner)
}

// CAPSTYPE_GLYPHCACHE (0x0010): 52 bytes
fn build_glyph_capability() -> Vec<u8> {
    let mut inner = vec![0u8; 40]; // glyphCache[10] x 4 bytes each
    write_u32_le(&mut inner, 0); // fragCache
    write_u16_le(&mut inner, 0); // glyphSupportLevel
    write_u16_le(&mut inner, 0); // pad2octets
    build_capability(CAPSTYPE_GLYPHCACHE, &inner)
}

// CAPSTYPE_VIRTUAL_CHANNEL (0x0014): 12 bytes
fn build_virtual_channel_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u32_le(&mut inner, 0); // flags
    write_u32_le(&mut inner, 1600); // vcChunkSize
    build_capability(CAPSTYPE_VIRTUAL_CHANNEL, &inner)
}

// CAPSETTYPE_COMPDESK (0x0019): 6 bytes
fn build_compdesk_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 1); // compDeskSupportLevel
    build_capability(CAPSETTYPE_COMPDESK, &inner)
}

// CAPSETTYPE_MULTIFRAGMENTUPDATE (0x001A): 8 bytes
fn build_multifragment_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u32_le(&mut inner, 4_128_768); // maxRequestSize
    build_capability(CAPSETTYPE_MULTIFRAGMENTUPDATE, &inner)
}

// CAPSETTYPE_LARGE_POINTER (0x001B): 6 bytes
fn build_large_pointer_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 1); // largePointerSupportFlags
    build_capability(CAPSETTYPE_LARGE_POINTER, &inner)
}

// CAPSETTYPE_SURFACE_COMMANDS (0x001C): 12 bytes
fn build_surface_commands_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    // Match grdp: accept surface bitmap commands and frame markers.
    write_u32_le(&mut inner, 0x0002 | 0x0010 | 0x0040);
    write_u32_le(&mut inner, 0); // reserved
    build_capability(CAPSETTYPE_SURFACE_COMMANDS, &inner)
}

// CAPSETTYPE_BITMAP_CODECS (0x001D): NSCodec support for surface commands.
fn build_bitmap_codecs_capability() -> Vec<u8> {
    // NSCodec GUID
    let nscodec_guid: [u8; 16] = [
        0xB9, 0x1B, 0x8D, 0xCA, 0x0F, 0x00, 0x4F, 0x15, 0x58, 0x9F, 0xAE, 0x2D, 0x1A, 0x87, 0xE2,
        0xD6,
    ];
    // NSCodec properties: fAllowDynamicFidelity=1, fAllowSubsampling=1, colorLossLevel=3
    let nscodec_props: [u8; 3] = [1, 1, 3];

    let mut inner = Vec::new();
    inner.push(1u8); // bitmapCodecCount
    inner.extend_from_slice(&nscodec_guid);
    inner.push(1u8); // codecID
    write_u16_le(&mut inner, nscodec_props.len() as u16);
    inner.extend_from_slice(&nscodec_props);
    build_capability(CAPSETTYPE_BITMAP_CODECS, &inner)
}

// CAPSSETTYPE_FRAME_ACKNOWLEDGE (0x001E): 8 bytes
fn build_frame_acknowledge_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u32_le(&mut inner, 2); // maxUnacknowledgedFrameCount
    build_capability(CAPSSETTYPE_FRAME_ACKNOWLEDGE, &inner)
}

/// Returns (capability_bytes, num_capabilities)
pub fn build_all_capabilities(width: u16, height: u16, kbd_layout: u32) -> (Vec<u8>, u16) {
    let mut caps = Vec::new();
    caps.extend_from_slice(&build_general_capability());
    caps.extend_from_slice(&build_bitmap_capability(width, height));
    caps.extend_from_slice(&build_order_capability());
    caps.extend_from_slice(&build_bitmapcache_rev2_capability());
    caps.extend_from_slice(&build_control_capability());
    caps.extend_from_slice(&build_activation_capability());
    caps.extend_from_slice(&build_pointer_capability());
    caps.extend_from_slice(&build_share_capability());
    caps.extend_from_slice(&build_colorcache_capability());
    caps.extend_from_slice(&build_sound_capability());
    caps.extend_from_slice(&build_input_capability(kbd_layout));
    caps.extend_from_slice(&build_font_capability());
    caps.extend_from_slice(&build_brush_capability());
    caps.extend_from_slice(&build_glyph_capability());
    caps.extend_from_slice(&build_virtual_channel_capability());
    caps.extend_from_slice(&build_compdesk_capability());
    caps.extend_from_slice(&build_multifragment_capability());
    caps.extend_from_slice(&build_large_pointer_capability());
    caps.extend_from_slice(&build_bitmap_codecs_capability());
    caps.extend_from_slice(&build_surface_commands_capability());
    caps.extend_from_slice(&build_frame_acknowledge_capability());
    let num = 21u16;
    (caps, num)
}
