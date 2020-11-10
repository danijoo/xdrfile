//! # xdrfile
//! Read and write xdr trajectory files in .xtc and .trr file format
//!
//! This crate is mainly intended to be a wrapper around the GROMACS libxdrfile
//! XTC library and provides basic functionality to read and write xtc and trr
//! files with a safe api.
//!
//! # Basic usage example
//! ```rust
//! use xdrfile::*;
//!
//! fn main() -> Result<()> {
//!     // get a handle to the file
//!     let mut trj = XTCTrajectory::open_read("tests/1l2y.xtc")?;
//!
//!     // find number of atoms in the file
//!     let num_atoms = trj.get_num_atoms()?;
//!
//!     // a frame object is used to get to read or write from a trajectory
//!     // without instantiating data arrays for every step
//!     let mut frame = Frame::with_capacity(num_atoms);
//!
//!     // read the first frame of the trajectory
//!     trj.read(&mut frame)?;
//!
//!     assert_eq!(frame.step, 1);
//!     assert_eq!(frame.num_atoms, num_atoms);
//!
//!     let first_atom_coords = frame.coords[0];
//!     assert_eq!(first_atom_coords, [-0.8901, 0.4127, -0.055499997]);
//!
//!     Ok(())
//! }
//! ```
//!
//! # Frame iteration
//! For convenience, the trajectory implementations provide "into_iter" to
//! be turned into an iterator that yields Rc<Frame>. If a frame is not kept
//! during iteration, the Iterator reuses it for better performance (and hence,
//! Rc is required)
//!
//! ```rust
//! use xdrfile::*;
//!
//! fn main() -> Result<()> {
//!     // get a handle to the file
//!     let trj = XTCTrajectory::open_read("tests/1l2y.xtc")?;
//!
//!     // iterate over all frames
//!     for (idx, result) in trj.into_iter().enumerate() {
//!         let frame = result?;
//!         println!("{}", frame.time);
//!         assert_eq!(idx+1, frame.step as usize);
//!     }
//!     Ok(())
//! }
//! ```

#[cfg(test)]
#[macro_use]
extern crate assert_approx_eq;
extern crate lazy_init;

pub mod c_abi;
mod errors;
mod frame;
mod iterator;
pub use errors::*;
pub use frame::Frame;
pub use iterator::*;

use c_abi::xdr_seek;
use c_abi::xdrfile;
use c_abi::xdrfile::XDRFILE;
use c_abi::xdrfile_trr;
use c_abi::xdrfile_xtc;

use lazy_init::Lazy;
use std::cell::Cell;
use std::ffi::CString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum FileMode {
    Write,
    Append,
    Read,
}

impl FileMode {
    /// Get a CStr slice corresponding to the file mode
    fn to_cstr(&self) -> &'static std::ffi::CStr {
        let bytes: &[u8; 2] = match *self {
            FileMode::Write => b"w\0",
            FileMode::Append => b"a\0",
            FileMode::Read => b"r\0",
        };

        std::ffi::CStr::from_bytes_with_nul(bytes).expect("CStr::from_bytes_with_nul failed")
    }
}

fn path_to_cstring(path: impl AsRef<Path>) -> Result<CString> {
    use ErrorKind::InvalidOsStr;
    let s = path
        .as_ref()
        .to_str()
        .ok_or_else(|| Error::from(InvalidOsStr))?;
    CString::new(s).map_err(|e| Error::from(e))
}

/// A safe wrapper around the c implementation of an XDRFile
struct XDRFile {
    xdrfile: *mut XDRFILE,
    #[allow(dead_code)]
    filemode: FileMode,
    path: PathBuf,
}

impl XDRFile {
    pub fn open(path: impl AsRef<Path>, filemode: FileMode) -> Result<XDRFile> {
        let path = path.as_ref();
        unsafe {
            let path_p = path_to_cstring(path)?.into_raw();
            // SAFETY: mode_p must not be mutated by the C code
            let mode_p = filemode.to_cstr().as_ptr();

            let xdrfile = xdrfile::xdrfile_open(path_p, mode_p);

            // Reconstitute the CString so it is deallocated correctly
            let _ = CString::from_raw(path_p);

            if !xdrfile.is_null() {
                let path = path.to_owned();
                Ok(XDRFile {
                    xdrfile,
                    filemode,
                    path,
                })
            } else {
                // Something went wrong. But the C api does not tell us what
                Err(Error::from((path, filemode)))
            }
        }
    }
}

impl Drop for XDRFile {
    /// Close the underlying xdr file on drop
    fn drop(&mut self) {
        unsafe {
            xdrfile::xdrfile_close(self.xdrfile);
        }
    }
}

/// The trajectory trait defines shared methods for xtc and trr trajectories
pub trait Trajectory {
    /// Read the next step of the trajectory into the frame object
    fn read(&mut self, frame: &mut Frame) -> Result<()>;

    /// Write the frame to the trajectory file
    fn write(&mut self, frame: &Frame) -> Result<()>;

    /// Flush the trajectory file
    fn flush(&mut self) -> Result<()>;

    /// Get the number of atoms from the give trajectory
    fn get_num_atoms(&mut self) -> Result<u32>;
}

/// Read/Write XTC Trajectories
pub struct XTCTrajectory {
    handle: XDRFile,
    precision: Cell<f32>, // internal mutability required for read method
    num_atoms: Lazy<Result<u32>>,
}

impl XTCTrajectory {
    pub fn open(path: impl AsRef<Path>, filemode: FileMode) -> Result<XTCTrajectory> {
        let xdr = XDRFile::open(path, filemode)?;
        Ok(XTCTrajectory {
            handle: xdr,
            precision: Cell::new(1000.0),
            num_atoms: Lazy::new(),
        })
    }

    /// Open a file in read mode
    pub fn open_read(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path, FileMode::Read)
    }

    /// Open a file in append mode
    pub fn open_append(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path, FileMode::Append)
    }

    /// Open a file in write mode
    pub fn open_write(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path, FileMode::Write)
    }
}

impl Trajectory for XTCTrajectory {
    fn read(&mut self, frame: &mut Frame) -> Result<()> {
        let mut step: i32 = 0;

        let num_atoms = self
            .get_num_atoms()
            .map_err(|e| e.with_task(ErrorTask::Read))? as usize;
        if num_atoms != frame.coords.len() {
            return Err(Error::from((&*frame, num_atoms)).with_task(ErrorTask::Read));
        }

        unsafe {
            // C lib requires an i32 to be passed, but step is exposed it as u32
            // (A step cannot be negative, can it?). So we need to create a step
            // variable to pass to read_xtc and cast it afterwards to u32
            let code = xdrfile_xtc::read_xtc(
                self.handle.xdrfile,
                num_atoms as i32,
                &mut step,
                &mut frame.time,
                &mut frame.box_vector,
                frame.coords.as_mut_ptr(),
                &mut self.precision.get(),
            ) as u32;
            frame.step = step as u32;
            Error::check_code(code, (), ErrorTask::Read)
        }
    }

    fn write(&mut self, frame: &Frame) -> Result<()> {
        unsafe {
            let code = xdrfile_xtc::write_xtc(
                self.handle.xdrfile,
                frame.num_atoms as i32,
                frame.step as i32,
                frame.time,
                frame.box_vector.as_ptr() as *mut [[f32; 3]; 3],
                frame.coords[..].as_ptr() as *mut [f32; 3],
                1000.0,
            ) as u32;
            Error::check_code(code, (), ErrorTask::Write)
        }
    }

    fn flush(&mut self) -> Result<()> {
        unsafe {
            let code = xdr_seek::xdr_flush(self.handle.xdrfile) as u32;
            Error::check_code(code, (), ErrorTask::Flush)
        }
    }

    fn get_num_atoms(&mut self) -> Result<u32> {
        self.num_atoms
            .get_or_create(|| {
                let mut num_atoms: i32 = 0;

                unsafe {
                    let path = path_to_cstring(&self.handle.path)?;
                    let path_p = path.into_raw();
                    let code =
                        xdrfile_xtc::read_xtc_natoms(path_p, &mut num_atoms as *const i32) as u32;
                    // Reconstitute the CString so it is deallocated correctly
                    let _ = CString::from_raw(path_p);

                    Error::check_code(code, num_atoms as u32, ErrorTask::ReadNumAtoms)
                }
            })
            .clone()
    }
}

/// Read/Write TRR Trajectories
pub struct TRRTrajectory {
    handle: XDRFile,
    num_atoms: Lazy<Result<u32>>,
}

impl TRRTrajectory {
    pub fn open(path: impl AsRef<Path>, filemode: FileMode) -> Result<TRRTrajectory> {
        let xdr = XDRFile::open(path, filemode)?;
        Ok(TRRTrajectory {
            handle: xdr,
            num_atoms: Lazy::new(),
        })
    }

    /// Open a file in read mode
    pub fn open_read(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path, FileMode::Read)
    }

    /// Open a file in append mode
    pub fn open_append(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path, FileMode::Append)
    }

    /// Open a file in write mode
    pub fn open_write(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path, FileMode::Write)
    }
}

impl Trajectory for TRRTrajectory {
    fn read(&mut self, frame: &mut Frame) -> Result<()> {
        let mut step: i32 = 0;
        let mut lambda: f32 = 0.0;

        let num_atoms = self
            .get_num_atoms()
            .map_err(|e| e.with_task(ErrorTask::Read))? as usize;
        if num_atoms != frame.coords.len() {
            return Err(Error::from((&*frame, num_atoms)).with_task(ErrorTask::Read));
        }

        unsafe {
            // C lib requires an i32 to be passed, but step is exposed it as u32
            // (A step cannot be negative, can it?). So we need to create a step
            // variable to pass to read_trr and cast it afterwards to u32.
            // Similar for lambda.
            let code = xdrfile_trr::read_trr(
                self.handle.xdrfile,
                num_atoms as i32,
                &mut step,
                &mut frame.time,
                &mut lambda,
                &mut frame.box_vector,
                frame.coords.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) as u32;
            frame.step = step as u32;
            Error::check_code(code, (), ErrorTask::Read)
        }
    }

    fn write(&mut self, frame: &Frame) -> Result<()> {
        unsafe {
            let code = xdrfile_trr::write_trr(
                self.handle.xdrfile,
                frame.num_atoms as i32,
                frame.step as i32,
                frame.time,
                0.0,
                frame.box_vector.as_ptr() as *mut [[f32; 3]; 3],
                frame.coords[..].as_ptr() as *mut [f32; 3],
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) as u32;
            Error::check_code(code, (), ErrorTask::Write)
        }
    }

    fn flush(&mut self) -> Result<()> {
        unsafe {
            let code = xdr_seek::xdr_flush(self.handle.xdrfile) as u32;
            Error::check_code(code, (), ErrorTask::Flush)
        }
    }

    fn get_num_atoms(&mut self) -> Result<u32> {
        self.num_atoms
            .get_or_create(|| {
                let mut num_atoms: i32 = 0;
                unsafe {
                    let path = path_to_cstring(&self.handle.path)?;
                    let path_p = path.into_raw();
                    let code =
                        xdrfile_trr::read_trr_natoms(path_p, &mut num_atoms as *const i32) as u32;
                    // Reconstitute the CString so it is deallocated correctly
                    let _ = CString::from_raw(path_p);

                    Error::check_code(code, num_atoms as u32, ErrorTask::ReadNumAtoms)
                }
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_write_xtc() -> Result<()> {
        let tempfile = NamedTempFile::new().expect("Could not create temporary file");
        let tmp_path = tempfile.path();

        let natoms: u32 = 2;
        let frame = Frame {
            num_atoms: natoms,
            step: 5,
            time: 2.0,
            box_vector: [[1.0, 2.0, 3.0], [2.0, 1.0, 3.0], [3.0, 2.0, 1.0]],
            coords: vec![[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        };
        let mut f = XTCTrajectory::open_write(&tmp_path)?;
        let write_status = f.write(&frame);
        match write_status {
            Err(_) => panic!("Failed"),
            Ok(()) => {}
        }
        f.flush()?;

        let mut new_frame = Frame::with_capacity(natoms);
        let mut f = XTCTrajectory::open_read(tmp_path)?;
        let num_atoms = f.get_num_atoms()?;
        assert_eq!(num_atoms, natoms);

        let read_status = f.read(&mut new_frame);
        match read_status {
            Err(e) => assert!(false, "{:?}", e),
            Ok(()) => {}
        }

        assert_eq!(new_frame.num_atoms, frame.num_atoms);
        assert_eq!(new_frame.step, frame.step);
        assert_approx_eq!(new_frame.time, frame.time);
        assert_eq!(new_frame.box_vector, frame.box_vector);
        assert_eq!(new_frame.coords, frame.coords);
        Ok(())
    }

    #[test]
    fn test_read_write_trr() -> Result<()> {
        let tempfile = NamedTempFile::new().expect("Could not create temporary file");
        let tmp_path = tempfile.path();

        let natoms: u32 = 2;
        let frame = Frame {
            num_atoms: natoms,
            step: 5,
            time: 2.0,
            box_vector: [[1.0, 2.0, 3.0], [2.0, 1.0, 3.0], [3.0, 2.0, 1.0]],
            coords: vec![[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        };
        let mut f = TRRTrajectory::open_write(tmp_path)?;
        let write_status = f.write(&frame);
        match write_status {
            Err(_) => panic!("Failed"),
            Ok(()) => {}
        }
        f.flush()?;

        let mut new_frame = Frame::with_capacity(natoms);
        let mut f = TRRTrajectory::open_read(tmp_path)?;
        // let num_atoms = f.get_num_atoms()?;
        // assert_eq!(num_atoms, natoms);

        let read_status = f.read(&mut new_frame);
        match read_status {
            Err(e) => assert!(false, "{:?}", e),
            Ok(()) => {}
        }

        assert_eq!(new_frame.num_atoms, frame.num_atoms);
        assert_eq!(new_frame.step, frame.step);
        assert_eq!(new_frame.time, frame.time);
        assert_eq!(new_frame.box_vector, frame.box_vector);
        assert_eq!(new_frame.coords, frame.coords);
        Ok(())
    }

    #[test]
    pub fn test_manual_loop() -> Result<(), Box<dyn std::error::Error>> {
        let mut xtc_frames = Vec::new();
        let mut xtc_traj = XTCTrajectory::open_read("tests/1l2y.xtc")?;
        let mut frame = Frame::with_capacity(xtc_traj.get_num_atoms()?);

        while let Ok(()) = xtc_traj.read(&mut frame) {
            xtc_frames.push(frame.clone());
        }

        let mut trr_frames = Vec::new();
        let mut trr_traj = TRRTrajectory::open_read("tests/1l2y.trr")?;

        while let Ok(()) = trr_traj.read(&mut frame) {
            trr_frames.push(frame.clone());
        }

        for (xtc, trr) in xtc_frames.into_iter().zip(trr_frames) {
            assert_eq!(xtc.num_atoms, trr.num_atoms);
            assert_eq!(xtc.step, trr.step);
            assert_eq!(xtc.time, trr.time);
            assert_eq!(xtc.box_vector, trr.box_vector);
            for (xtc_xyz, trr_xyz) in xtc.coords.into_iter().zip(trr.coords) {
                assert!(xtc_xyz[0] - trr_xyz[0] <= 1e-5);
                assert!(xtc_xyz[1] - trr_xyz[1] <= 1e-5);
                assert!(xtc_xyz[2] - trr_xyz[2] <= 1e-5);
            }
        }
        Ok(())
    }

    #[test]
    pub fn test_wrong_size_frame() -> Result<(), Box<dyn std::error::Error>> {
        let mut xtc_traj = XTCTrajectory::open_read("tests/1l2y.xtc")?;
        let mut frame = Frame::new();

        let result = xtc_traj.read(&mut frame);
        if let Err(e) = result {
            assert_eq!(e.task(), &Some(ErrorTask::Read));
            assert!(if let ErrorKind::WrongSizeFrame { .. } = e.kind() {
                true
            } else {
                false
            });
        } else {
            panic!("A read with an incorrectly sized frame should not succeed")
        }
        Ok(())
    }

    #[test]
    fn test_path_to_cstring() -> Result<(), Box<dyn std::error::Error>> {
        let result_invalid = path_to_cstring(PathBuf::from("invalid/\0path"));

        if let Err(err) = result_invalid {
            match err.kind() {
                ErrorKind::NullInStr(_) => (),
                ErrorKind::InvalidOsStr => (),
                _ => panic!("Improper error type for path_to_cstring"),
            }
        } else {
            panic!("path_to_cstring should return Err if there are null bytes");
        }

        let result_valid = path_to_cstring("valid/path");
        assert_eq!(result_valid, Ok(CString::new("valid/path")?));

        Ok(())
    }

    #[test]
    fn test_err_could_not_open() {
        let file_name = "non-existent.xtc";

        let path = Path::new(&file_name);
        if let Err(e) = XDRFile::open(file_name, FileMode::Read) {
            if let ErrorKind::CouldNotOpen {
                path: err_path,
                mode: err_mode,
            } = e.kind()
            {
                assert_eq!(path, err_path);
                assert_eq!(FileMode::Read, *err_mode)
            } else {
                panic!("Wrong Error type")
            }
        }
    }

    #[test]
    fn test_err_could_not_read_atom_nr() -> Result<()> {
        let file_name = "README.md"; // not a trajectory
        let mut trr = TRRTrajectory::open_read(file_name)?;
        if let Err(e) = trr.get_num_atoms() {
            assert_eq!(Some(ErrorCode::ExdrMagic), e.code());
        } else {
            panic!("Should not be able to read number of atoms from readme");
        }
        Ok(())
    }

    #[test]
    fn test_err_could_not_read() -> Result<()> {
        let file_name = "README.md"; // not a trajectory
        let mut frame = Frame::with_capacity(1);
        let mut trr = TRRTrajectory::open_read(file_name)?;
        if let Err(e) = trr.read(&mut frame) {
            assert_eq!(Some(ErrorCode::ExdrMagic), e.code());
        } else {
            panic!("Should not be able to read number of atoms from readme");
        }
        Ok(())
    }

    #[test]
    fn test_err_file_eof() -> Result<(), Box<dyn std::error::Error>> {
        let tempfile = NamedTempFile::new()?;
        let tmp_path = tempfile.path();

        let natoms: u32 = 2;
        let frame = Frame {
            num_atoms: natoms,
            step: 5,
            time: 2.0,
            box_vector: [[1.0, 2.0, 3.0], [2.0, 1.0, 3.0], [3.0, 2.0, 1.0]],
            coords: vec![[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        };
        let mut f = XTCTrajectory::open_write(&tmp_path)?;
        f.write(&frame)?;
        f.flush()?;

        let mut new_frame = Frame::with_capacity(natoms);
        let mut f = XTCTrajectory::open_read(tmp_path)?;

        f.read(&mut new_frame)?;

        let result = f.read(&mut new_frame); // Should be eof as we only wrote one frame
        if let Err(e) = result {
            assert!(e.is_eof());
        } else {
            panic!("read two frames after writing one");
        }

        let mut file = std::fs::File::create(tmp_path)?;
        use std::io::Write as _;
        file.write_all(&[0; 999])?;
        file.flush()?;

        let mut f = XTCTrajectory::open_read(tmp_path)?;
        let result = f.read(&mut new_frame); // Should be an invalid XTC file
        if let Err(e) = result {
            assert!(!e.is_eof());
        } else {
            panic!("999 zero bytes was read as a valid XTC file");
        }

        Ok(())
    }
}
