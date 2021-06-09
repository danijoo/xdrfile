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
//!     let mut frame = Frame::with_len(num_atoms);
//!
//!     // read the first frame of the trajectory
//!     trj.read(&mut frame)?;
//!
//!     assert_eq!(frame.step, 1);
//!     assert_eq!(frame.len(), num_atoms);
//!
//!     let first_atom_coords = frame[0];
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
//!         assert_eq!(idx+1, frame.step);
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
use std::convert::{TryFrom, TryInto};
use std::ffi::CString;
use std::io;
use std::io::SeekFrom;
use std::marker::PhantomData;
use std::os::raw::{c_float, c_int};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

/// File Mode for accessing trajectories.
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
    if let Some(s) = path.as_ref().to_str() {
        CString::new(s).map_err(|e| Error::InvalidOsStr(Some(e)))
    } else {
        Err(Error::InvalidOsStr(None))
    }
}

fn to<I, O>(value: I, task: ErrorTask, name: &'static str) -> Result<O>
where
    I: TryInto<O> + std::fmt::Display + Copy,
{
    value.try_into().map_err(|_| Error::OutOfRange {
        name,
        value: format!("{}", &value),
        target: std::any::type_name::<O>(),
        task,
    })
}

macro_rules! to {
    ($value:expr, $task:expr) => {
        to($value, $task, stringify!($value))
    };
}

/// Convert an error code from a C call to an Error
///
/// `code` should be an integer return code returned from the C API.
/// If `code` indicates the function returned successfully, None is returned;
/// otherwise, the code is converted into the appropriate `Error`.
fn check_code(code: impl Into<ErrorCode>, task: ErrorTask) -> Option<Error> {
    let code: ErrorCode = code.into();
    if let ErrorCode::ExdrOk = code {
        None
    } else {
        Some(Error::from((code, task)))
    }
}

/// A safe wrapper around the c implementation of an XDRFile
struct XDRFile {
    xdrfile: NonNull<XDRFILE>,
    _owned: PhantomData<XDRFILE>,
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

            if let Some(xdrfile) = NonNull::new(xdrfile) {
                let path = path.to_owned();
                Ok(XDRFile {
                    xdrfile,
                    _owned: PhantomData,
                    filemode,
                    path,
                })
            } else {
                // Something went wrong. But the C api does not tell us what
                Err((path, filemode).into())
            }
        }
    }

    /// Get the current position in the file
    pub fn tell(&self) -> u64 {
        unsafe {
            xdr_seek::xdr_tell(self.xdrfile.as_ptr())
                .try_into()
                .expect("i64 could not be converted to u64")
        }
    }
}

impl io::Seek for XDRFile {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let (whence, pos) = match pos {
            SeekFrom::Start(u) => (
                0,
                i64::try_from(u).expect("Seek position did not fit in i64"),
            ),
            SeekFrom::Current(i) => (1, i),
            SeekFrom::End(i) => (2, i),
        };
        unsafe {
            let code = xdr_seek::xdr_seek(self.xdrfile.as_ptr(), pos, whence);
            match check_code(code, ErrorTask::Seek) {
                None => Ok(self.tell()),
                Some(err) => Err(io::Error::new(io::ErrorKind::Other, err)),
            }
        }
    }
}

impl Drop for XDRFile {
    /// Close the underlying xdr file on drop
    fn drop(&mut self) {
        unsafe {
            xdrfile::xdrfile_close(self.xdrfile.as_ptr());
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
    fn get_num_atoms(&mut self) -> Result<usize>;

}

/// Handle to Read/Write XTC Trajectories
pub struct XTCTrajectory {
    handle: XDRFile,
    precision: Cell<c_float>, // internal mutability required for read method
    num_atoms: Lazy<Result<usize>>,
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
        let mut step: c_int = 0;

        let num_atoms = self
            .get_num_atoms()
            .map_err(|e| Error::CouldNotCheckNAtoms(Box::new(e)))?;
        if num_atoms != frame.coords.len() {
            return Err((&*frame, num_atoms).into());
        }

        unsafe {
            let code = xdrfile_xtc::read_xtc(
                self.handle.xdrfile.as_ptr(),
                to!(num_atoms, ErrorTask::Read)?,
                &mut step,
                &mut frame.time,
                &mut frame.box_vector,
                frame.coords.as_mut_ptr(),
                &mut self.precision.get(),
            );
            if let Some(err) = check_code(code, ErrorTask::Read) {
                return Err(err);
            }
            frame.step = to!(step, ErrorTask::Read)?;
            Ok(())
        }
    }

    fn write(&mut self, frame: &Frame) -> Result<()> {
        unsafe {
            let code = xdrfile_xtc::write_xtc(
                self.handle.xdrfile.as_ptr(),
                to!(frame.num_atoms(), ErrorTask::Write)?,
                to!(frame.step, ErrorTask::Write)?,
                frame.time,
                &frame.box_vector,
                frame.coords.as_ptr(),
                1000.0,
            );
            if let Some(err) = check_code(code, ErrorTask::Write) {
                Err(err)
            } else {
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> Result<()> {
        unsafe {
            let code = xdr_seek::xdr_flush(self.handle.xdrfile.as_ptr());
            if let Some(err) = check_code(code, ErrorTask::Flush) {
                Err(err)
            } else {
                Ok(())
            }
        }
    }

    fn get_num_atoms(&mut self) -> Result<usize> {
        self.num_atoms
            .get_or_create(|| {
                let mut num_atoms: c_int = 0;

                unsafe {
                    let path = path_to_cstring(&self.handle.path)?;
                    let path_p = path.into_raw();
                    let code = xdrfile_xtc::read_xtc_natoms(path_p, &mut num_atoms);
                    // Reconstitute the CString so it is deallocated correctly
                    let _ = CString::from_raw(path_p);

                    if let Some(err) = check_code(code, ErrorTask::ReadNumAtoms) {
                        Err(err)
                    } else {
                        to!(num_atoms, ErrorTask::ReadNumAtoms)
                    }
                }
            })
            .clone()
    }
}

impl XTCTrajectory {
    /// Get the current position in the file
    pub fn tell(&self) -> u64 {
        self.handle.tell()
    }
}

impl io::Seek for XTCTrajectory {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.handle.seek(pos)
    }
}

/// Handle to Read/Write TRR Trajectories
pub struct TRRTrajectory {
    handle: XDRFile,
    num_atoms: Lazy<Result<usize>>,
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
        let mut step: c_int = 0;
        let mut lambda: c_float = 0.0;

        let num_atoms = self
            .get_num_atoms()
            .map_err(|e| Error::CouldNotCheckNAtoms(Box::new(e)))?;
        if num_atoms != frame.coords.len() {
            return Err((&*frame, num_atoms).into());
        }

        unsafe {
            let code = xdrfile_trr::read_trr(
                self.handle.xdrfile.as_ptr(),
                to!(num_atoms, ErrorTask::Read)?,
                &mut step,
                &mut frame.time,
                &mut lambda,
                &mut frame.box_vector,
                frame.coords.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if let Some(err) = check_code(code, ErrorTask::Read) {
                return Err(err);
            }
            frame.step = to!(step, ErrorTask::Read)?;
            Ok(())
        }
    }

    fn write(&mut self, frame: &Frame) -> Result<()> {
        unsafe {
            let code = xdrfile_trr::write_trr(
                self.handle.xdrfile.as_ptr(),
                to!(frame.len(), ErrorTask::Write)?,
                to!(frame.step, ErrorTask::Write)?,
                frame.time,
                0.0,
                &frame.box_vector,
                frame.coords[..].as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if let Some(err) = check_code(code, ErrorTask::Write) {
                Err(err)
            } else {
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> Result<()> {
        unsafe {
            let code = xdr_seek::xdr_flush(self.handle.xdrfile.as_ptr());
            if let Some(err) = check_code(code, ErrorTask::Flush) {
                Err(err)
            } else {
                Ok(())
            }
        }
    }

    fn get_num_atoms(&mut self) -> Result<usize> {
        self.num_atoms
            .get_or_create(|| {
                let mut num_atoms: c_int = 0;
                unsafe {
                    let path = path_to_cstring(&self.handle.path)?;
                    let path_p = path.into_raw();
                    let code = xdrfile_trr::read_trr_natoms(path_p, &mut num_atoms);
                    // Reconstitute the CString so it is deallocated correctly
                    let _ = CString::from_raw(path_p);

                    if let Some(err) = check_code(code, ErrorTask::ReadNumAtoms) {
                        Err(err)
                    } else {
                        to!(num_atoms, ErrorTask::ReadNumAtoms)
                    }
                }
            })
            .clone()
    }
}

impl TRRTrajectory {
    /// Get the current position in the file
    pub fn tell(&self) -> u64 {
        self.handle.tell()
    }
}

impl io::Seek for TRRTrajectory {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.handle.seek(pos)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::io::Seek;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_write_append_read_xtc() -> Result<()> {
        let tempfile = NamedTempFile::new().expect("Could not create temporary file");
        let tmp_path = tempfile.path();
        let natoms = 2;

        // write frame 1
        let frame = Frame {
            step: 1,
            time: 1.0,
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

        // append frame 2
        let frame2 = Frame {
            step: 2,
            time: 2.0,
            box_vector: [[1.0, 2.0, 3.0], [2.0, 1.0, 3.0], [3.0, 2.0, 1.0]],
            coords: vec![[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        };
        let mut f = XTCTrajectory::open_append(&tmp_path)?;
        let write_status = f.write(&frame2);
        match write_status {
            Err(_) => panic!("Failed"),
            Ok(()) => {}
        }
        f.flush()?;

        // open trj for read
        let mut new_frame = Frame::with_len(natoms);
        let mut f = XTCTrajectory::open_read(tmp_path)?;
        let num_atoms = f.get_num_atoms()?;
        assert_eq!(num_atoms, natoms);

        // check frame 1 ...
        let read_status = f.read(&mut new_frame);
        match read_status {
            Err(e) => assert!(false, "{:?}", e),
            Ok(()) => {}
        }

        assert_eq!(new_frame.len(), frame.len());
        assert_eq!(new_frame.step, frame.step);
        assert_approx_eq!(new_frame.time, frame.time);
        assert_eq!(new_frame.box_vector, frame.box_vector);
        assert_eq!(new_frame.coords, frame.coords);

        // and check frame 1 ...
        let read_status = f.read(&mut new_frame);
        match read_status {
            Err(e) => assert!(false, "{:?}", e),
            Ok(()) => {}
        }

        assert_eq!(new_frame.len(), frame2.len());
        assert_eq!(new_frame.step, frame2.step);
        assert_approx_eq!(new_frame.time, frame2.time);
        assert_eq!(new_frame.box_vector, frame2.box_vector);
        assert_eq!(new_frame.coords, frame2.coords);
        Ok(())
    }

    #[test]
    fn test_write_append_read_trr() -> Result<()> {
        let tempfile = NamedTempFile::new().expect("Could not create temporary file");
        let tmp_path = tempfile.path();
        let natoms = 2;

        // write frame 1
        let frame = Frame {
            step: 1,
            time: 1.0,
            box_vector: [[1.0, 2.0, 3.0], [2.0, 1.0, 3.0], [3.0, 2.0, 1.0]],
            coords: vec![[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        };
        let mut f = TRRTrajectory::open_write(&tmp_path)?;
        let write_status = f.write(&frame);
        match write_status {
            Err(_) => panic!("Failed"),
            Ok(()) => {}
        }
        f.flush()?;

        // append frame 2
        let frame2 = Frame {
            step: 2,
            time: 2.0,
            box_vector: [[1.0, 2.0, 3.0], [2.0, 1.0, 3.0], [3.0, 2.0, 1.0]],
            coords: vec![[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        };
        let mut f = TRRTrajectory::open_append(&tmp_path)?;
        let write_status = f.write(&frame2);
        match write_status {
            Err(_) => panic!("Failed"),
            Ok(()) => {}
        }
        f.flush()?;

        // open trj for read
        let mut new_frame = Frame::with_len(natoms);
        let mut f = TRRTrajectory::open_read(tmp_path)?;
        let num_atoms = f.get_num_atoms()?;
        assert_eq!(num_atoms, natoms);

        // check frame 1 ...
        let read_status = f.read(&mut new_frame);
        match read_status {
            Err(e) => assert!(false, "{:?}", e),
            Ok(()) => {}
        }

        assert_eq!(new_frame.len(), frame.len());
        assert_eq!(new_frame.step, frame.step);
        assert_approx_eq!(new_frame.time, frame.time);
        assert_eq!(new_frame.box_vector, frame.box_vector);
        assert_eq!(new_frame.coords, frame.coords);

        // and check frame 1 ...
        let read_status = f.read(&mut new_frame);
        match read_status {
            Err(e) => assert!(false, "{:?}", e),
            Ok(()) => {}
        }

        assert_eq!(new_frame.len(), frame2.len());
        assert_eq!(new_frame.step, frame2.step);
        assert_approx_eq!(new_frame.time, frame2.time);
        assert_eq!(new_frame.box_vector, frame2.box_vector);
        assert_eq!(new_frame.coords, frame2.coords);
        Ok(())
    }

    #[test]
    pub fn test_manual_loop() -> Result<(), Box<dyn std::error::Error>> {
        let mut xtc_frames = Vec::new();
        let mut xtc_traj = XTCTrajectory::open_read("tests/1l2y.xtc")?;
        let mut frame = Frame::with_len(xtc_traj.get_num_atoms()?);

        while let Ok(()) = xtc_traj.read(&mut frame) {
            xtc_frames.push(frame.clone());
        }

        let mut trr_frames = Vec::new();
        let mut trr_traj = TRRTrajectory::open_read("tests/1l2y.trr")?;

        while let Ok(()) = trr_traj.read(&mut frame) {
            trr_frames.push(frame.clone());
        }

        for (xtc, trr) in xtc_frames.into_iter().zip(trr_frames) {
            assert_eq!(xtc.len(), trr.len());
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
            assert!(matches!(e, Error::WrongSizeFrame { .. }));
        } else {
            panic!("A read with an incorrectly sized frame should not succeed")
        }
        Ok(())
    }

    #[test]
    fn test_path_to_cstring() -> Result<(), Box<dyn std::error::Error>> {
        // A valid string should convert to CString successfully
        let valid_result = path_to_cstring(PathBuf::from("test"));
        match valid_result {
            Ok(s) => {
                assert_eq!(s, CString::new("test")?);
            }
            Err(_) => panic!("Valid Path failed to convert to CString."),
        }

        // \0 in path should result in an InvalidOsStr(Some(NulError))
        let result = path_to_cstring(PathBuf::from("invalid/\0path"));
        match result {
            Ok(_) => panic!("Cstring conversion did not fail"),
            Err(e) => match e {
                Error::InvalidOsStr(opt) => assert!(opt.is_some()),
                _ => panic!("Wrong error type. (This should never happend)."),
            },
        }
        Ok(())
    }

    #[test]
    fn test_tell() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tempfile = NamedTempFile::new()?;
        let tmp_path = tempfile.path();

        let natoms: usize = 2;
        let frame = Frame {
            step: 5,
            time: 2.0,
            box_vector: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            coords: vec![[0.0, 0.0, 0.0], [0.5, 0.5, 0.5]],
        };
        let mut f = TRRTrajectory::open_write(tmp_path)?;
        assert_eq!(f.tell(), 0);
        f.write(&frame)?;
        assert_eq!(f.tell(), 144);
        f.flush()?;

        let mut new_frame = Frame::with_len(natoms);
        let mut f = TRRTrajectory::open_read(tmp_path)?;
        assert_eq!(f.tell(), 0);

        f.read(&mut new_frame)?;
        assert_eq!(f.tell(), 144);

        Ok(())
    }

    #[test]
    fn test_seek() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tempfile = NamedTempFile::new()?;
        let tmp_path = tempfile.path();

        let natoms: usize = 2;
        let mut frame = Frame {
            step: 0,
            time: 0.0,
            box_vector: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            coords: vec![[0.0, 0.0, 0.0], [0.5, 0.5, 0.5]],
        };
        let mut f = TRRTrajectory::open_write(tmp_path)?;
        f.write(&frame)?;
        let after_first_frame = f.tell();
        frame.step += 1;
        frame.time += 10.0;
        f.write(&frame)?;
        let after_second_frame = f.tell();
        f.flush()?;

        let mut new_frame = Frame::with_len(natoms);
        let mut f = TRRTrajectory::open_read(tmp_path)?;
        let pos = f.seek(std::io::SeekFrom::Current(144))?;
        assert_eq!(pos, after_first_frame);

        f.read(&mut new_frame)?;
        assert_eq!(f.tell(), after_second_frame);

        assert_eq!(new_frame.len(), frame.len());
        assert_eq!(new_frame.step, frame.step);
        assert_eq!(new_frame.time, frame.time);
        assert_eq!(new_frame.box_vector, frame.box_vector);
        assert_eq!(new_frame.coords, frame.coords);

        Ok(())
    }

    #[test]
    fn test_err_could_not_open() {
        let file_name = "non-existent.xtc";

        let path = Path::new(&file_name);
        if let Err(e) = XDRFile::open(file_name, FileMode::Read) {
            if let Error::CouldNotOpen {
                path: err_path,
                mode: err_mode,
            } = e
            {
                assert_eq!(path, err_path);
                assert_eq!(FileMode::Read, err_mode)
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
        let mut frame = Frame::with_len(1);
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

        let natoms = 2;
        let frame = Frame {
            step: 5,
            time: 2.0,
            box_vector: [[1.0, 2.0, 3.0], [2.0, 1.0, 3.0], [3.0, 2.0, 1.0]],
            coords: vec![[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        };
        let mut f = XTCTrajectory::open_write(&tmp_path)?;
        f.write(&frame)?;
        f.flush()?;

        let mut new_frame = Frame::with_len(natoms);
        let mut f = XTCTrajectory::open_read(tmp_path)?;

        f.read(&mut new_frame)?;

        let result = f.read(&mut new_frame); // Should be eof as we only wrote one frame
        if let Err(e) = result {
            assert!(e.is_eof());
        } else {
            panic!("read two frames after writing one");
        }

        let mut file = std::fs::File::create(tmp_path)?;
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

    #[test]
    fn test_check_code() {
        let code: ErrorCode = 0.into();
        assert!(!check_code(code, ErrorTask::Read).is_some());

        for i in vec![1, 10, 100, 1000] {
            let code: ErrorCode = i.into();
            assert!(check_code(code, ErrorTask::Read).is_some());
        }
    }

    #[test]
    fn test_to() -> Result<()> {
        assert_eq!(24234_i32, to!(24234_usize, ErrorTask::Write)?);

        let big_number = 3_294_967_295_usize;
        let expected: Result<i32> = Err(Error::OutOfRange {
            name: "big_number",
            task: ErrorTask::Write,
            value: "3294967295".to_string(),
            target: "i32",
        });
        assert_eq!(expected, to!(big_number, ErrorTask::Write));

        let num_atoms: usize = 304;
        let res: Result<u8, _> = to!(num_atoms, ErrorTask::Write);
        assert_eq!(
            format!("{}", res.unwrap_err()),
            "Illegal num_atoms while writing trajectory: Failed to cast 304 to u8"
        );

        Ok(())
    }

    #[test]
    fn test_write_outofrange_step() -> Result<(), Box<dyn std::error::Error>> {
        let tempfile = NamedTempFile::new()?;
        let tmp_path = tempfile.path();
        let mut traj = XTCTrajectory::open_write(tmp_path)?;

        let frame = Frame {
            step: usize::MAX,
            time: 0.0,
            box_vector: [[0.0; 3]; 3],
            coords: vec![[1.0; 3]],
        };
        let expected = Error::OutOfRange {
            name: "frame.step",
            value: usize::MAX.to_string(),
            target: "i32",
            task: ErrorTask::Write,
        };

        if let Err(e) = traj.write(&frame) {
            print!("{:?}", e);
            assert_eq!(expected, e);
        } else {
            panic!("Writing frame with step=usize::MAX should not succeed.")
        }

        Ok(())
    }
}
