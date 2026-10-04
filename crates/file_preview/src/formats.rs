use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Image,
    Video,
    Model,
    Audio,
    Unreal,
    Max,
    Sheet,
    Word,
    Pdf,
    Wps,
    Presentation,
    Binary,
    Sqlite,
}

pub fn classify(path: &Path) -> Option<FileKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "jpe" | "jfif" | "webp" | "gif" | "apng" | "bmp" | "dib"
        | "tif" | "tiff" | "ico" | "cur" | "svg" | "avif" | "heic" | "heif" | "jxl" | "tga"
        | "dds" | "exr" | "hdr" | "pic" | "psd" | "psb" | "qoi" | "ppm" | "pgm" | "pbm" | "pnm"
        | "pam" | "pcx" | "jp2" | "j2k" | "jpf" | "jpx" | "jpm" | "mj2" | "dpx" | "cin" | "sgi"
        | "rgb" | "rgba" | "ras" | "xbm" | "xpm" | "cr2" | "cr3" | "nef" | "arw" | "dng"
        | "orf" | "rw2" | "raf" | "ktx" | "ktx2" | "pvr" => Some(FileKind::Image),
        "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "wmv" | "asf" | "flv" | "f4v" | "mpg"
        | "mpeg" | "mpe" | "m2v" | "mxf" | "mts" | "m2ts" | "vob" | "ogv" | "ogg" | "3gp"
        | "3g2" | "rm" | "rmvb" | "bik" | "bk2" => Some(FileKind::Video),
        "glb" | "gltf" | "fbx" | "obj" | "stl" | "ply" | "dae" | "3ds" | "3mf" | "amf" | "usd"
        | "usda" | "usdc" | "usdz" | "blend" | "dxf" | "lwo" | "lws" | "ase" | "ac" | "ms3d"
        | "cob" | "scn" | "md2" | "md3" | "md5mesh" | "mdc" | "mdl" | "nff" | "off" | "raw"
        | "smd" | "vta" | "x" | "x3d" | "wrl" | "vrml" | "ifc" | "irr" | "irrmesh" | "b3d"
        | "q3d" | "q3s" | "ndo" | "ter" | "hmp" | "csm" | "bvh" | "vtk" | "vtp" | "pcd" | "xyz"
        | "gcode" | "vox" => Some(FileKind::Model),
        "wav" | "mp3" | "flac" | "aac" | "m4a" | "aif" | "aiff" | "opus" | "wma" | "oga"
        | "amr" => Some(FileKind::Audio),
        "uasset" | "umap" => Some(FileKind::Unreal),
        "max" => Some(FileKind::Max),
        "xlsx" | "xlsm" | "xltx" | "xltm" | "xlsb" | "xls" | "xlt" | "ods" => Some(FileKind::Sheet),
        "docx" | "docm" | "dotx" | "dotm" => Some(FileKind::Word),
        "pdf" => Some(FileKind::Pdf),
        "wps" | "wpt" | "et" | "ett" | "dps" | "dpt" => Some(FileKind::Wps),
        "pptx" | "pptm" | "potx" | "potm" | "ppsx" | "ppsm" | "ppt" | "pot" | "pps" => {
            Some(FileKind::Presentation)
        }
        "exe" | "dll" | "dylib" | "pdb" | "lib" | "a" => Some(FileKind::Binary),
        "db" | "sqlite" | "sqlite3" | "db3" | "s3db" => Some(FileKind::Sqlite),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_p4j_formats_case_insensitively() {
        for extension in [
            "PNG", "JPG", "JPEG", "JPE", "JFIF", "WEBP", "GIF", "APNG", "BMP", "DIB", "TIF",
            "TIFF", "ICO", "CUR", "SVG", "AVIF", "HEIC", "HEIF", "JXL", "TGA", "DDS", "EXR", "HDR",
            "PIC", "PSD", "PSB", "QOI", "PPM", "PGM", "PBM", "PNM", "PAM", "PCX", "JP2", "J2K",
            "JPF", "JPX", "JPM", "MJ2", "DPX", "CIN", "SGI", "RGB", "RGBA", "RAS", "XBM", "XPM",
            "CR2", "CR3", "NEF", "ARW", "DNG", "ORF", "RW2", "RAF", "KTX", "KTX2", "PVR",
        ] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Image)
            );
        }
        for extension in [
            "MP4", "M4V", "MOV", "WEBM", "MKV", "AVI", "WMV", "ASF", "FLV", "F4V", "MPG", "MPEG",
            "MPE", "M2V", "MXF", "MTS", "M2TS", "VOB", "OGV", "OGG", "3GP", "3G2", "RM", "RMVB",
            "BIK", "BK2",
        ] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Video)
            );
        }
        for extension in [
            "GLB", "GLTF", "FBX", "OBJ", "STL", "PLY", "DAE", "3DS", "3MF", "AMF", "USD", "USDA",
            "USDC", "USDZ", "BLEND", "DXF", "LWO", "LWS", "ASE", "AC", "MS3D", "COB", "SCN", "MD2",
            "MD3", "MD5MESH", "MDC", "MDL", "NFF", "OFF", "RAW", "SMD", "VTA", "X", "X3D", "WRL",
            "VRML", "IFC", "IRR", "IRRMESH", "B3D", "Q3D", "Q3S", "NDO", "TER", "HMP", "CSM",
            "BVH", "VTK", "VTP", "PCD", "XYZ", "GCODE", "VOX",
        ] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Model)
            );
        }
        for extension in [
            "WAV", "MP3", "FLAC", "AAC", "M4A", "AIF", "AIFF", "OPUS", "WMA", "OGA", "AMR",
        ] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Audio)
            );
        }
        for extension in ["UASSET", "UMAP"] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Unreal)
            );
        }
        {
            let extension = "MAX";
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Max)
            );
        }
        for extension in ["XLSX", "XLSM", "XLTX", "XLTM", "XLSB", "XLS", "XLT", "ODS"] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Sheet)
            );
        }
        for extension in ["DOCX", "DOCM", "DOTX", "DOTM"] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Word)
            );
        }
        {
            let extension = "PDF";
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Pdf)
            );
        }
        for extension in ["WPS", "WPT", "ET", "ETT", "DPS", "DPT"] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Wps)
            );
        }
        for extension in [
            "PPTX", "PPTM", "POTX", "POTM", "PPSX", "PPSM", "PPT", "POT", "PPS",
        ] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Presentation)
            );
        }
        for extension in ["EXE", "DLL", "DYLIB", "PDB", "LIB", "A"] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Binary)
            );
        }
        for extension in ["DB", "SQLITE", "SQLITE3", "DB3", "S3DB"] {
            assert_eq!(
                classify(Path::new(&format!("file.{extension}"))),
                Some(FileKind::Sqlite)
            );
        }
        assert_eq!(classify(Path::new("file.rs")), None);
    }
}
