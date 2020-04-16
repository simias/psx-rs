use std::fmt;

use serde::ser::{Serializer, Serialize, SerializeSeq};
use serde::de::{Deserialize, Deserializer, Visitor, SeqAccess, Error};

use memory::Addressable;
use cdrom::disc::Region;

use self::db::Metadata;

pub mod db;

/// BIOS image
pub struct Bios {
    /// BIOS memory. Boxed in order not to overflow the stack at the
    /// construction site. Might change once "placement new" is
    /// available.
    data: Box<[u8; BIOS_SIZE]>,
    metadata: &'static Metadata,
}

impl Bios {

    /// Create a BIOS image from `binary` and attempt to match it with
    /// an entry in the database. If no match can be found return
    /// `None`.
    pub fn new(binary: Box<[u8; BIOS_SIZE]>) -> Option<Bios> {
        match db::lookup_blob(&*binary) {
            Some(metadata) => Some(Bios {
                data: binary,
                metadata: metadata,
            }),
            None => None,
        }
    }

    /// Generate a dummy BIOS that won't work, used for
    /// deserialization and running unit tests
    pub fn dummy() -> Bios {
        let mut bios =
            Bios {
                data: box_array![0; BIOS_SIZE],
                metadata: &DUMMY_METADATA,
            };

        // Store `0x7badb105` (an invalid instruction) in the BIOS
        // for troubleshooting.
        for (i, b) in bios.data.iter_mut().enumerate() {
            *b = (0x7badb105 >> ((i % 4) * 2)) as u8;
        }

        bios
    }

    /// Attempt to modify the BIOS ROM to remove the call to the code
    /// responsible for the boot logo animations (SCEx/PS) and
    /// directly boot the game. This can break some games!  Returns
    /// `Ok(())` if the code was patched, `Err(())` if we don't know
    /// how to hack this particular BIOS.
    pub fn patch_boot_animation(&mut self) -> Result<(), ()> {
        // Set the logo jump to `0` (NOP)
        self.patch_animation_jump_hook(0)
    }

    /// Attempt to modify the BIOS ROM to replace the call to the code
    /// responsible for the boot logo animations by the provided
    /// instruction.
    pub fn patch_animation_jump_hook(&mut self,
                                     instruction: u32) -> Result<(), ()> {
        match self.metadata.animation_jump_hook {
            Some(h) => {
                let h = h as usize;

                self.data[h]     = instruction as u8;
                self.data[h + 1] = (instruction >> 8) as u8;
                self.data[h + 2] = (instruction >> 16) as u8;
                self.data[h + 3] = (instruction >> 24) as u8;

                Ok(())
            }
            None => Err(())
        }
    }

    /// Attempt to modify the BIOS ROM to enable the debug UART
    /// output. Returns `Err(())` if we couldn't patch the BIOS.
    pub fn enable_debug_uart(&mut self) -> Result<(), ()> {
        match self.metadata.patch_debug_uart {
            Some(patch) => {
                patch(self);
                Ok(())
            },
            None => Err(()),
        }
    }

    /// fetch the little endian value at `offset`
    pub fn load<T: Addressable>(&self, offset: u32) -> u32 {
        let offset = offset as usize;

        let mut r = 0;

        for i in 0..T::size() as usize {
            r |= (self.data[offset + i] as u32) << (8 * i)
        }

        r
    }

    /// Return a static pointer to the BIOS's Metadata
    pub fn metadata(&self) -> &'static Metadata {
        self.metadata
    }
}


impl Serialize for Bios {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let sha256 = &self.metadata.sha256;

            let mut seq = serializer.serialize_seq(Some(sha256.len()))?;
            for e in sha256 {
                seq.serialize_element(e)?;
            }
            seq.end()
        }
}

impl<'de> Deserialize<'de> for Bios {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de> 
        { 
            struct Sha256Visitor;

            impl<'de> Visitor<'de> for Sha256Visitor
            {
                type Value = [u8; 32];

                fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    formatter.write_str("an array of length 32")
                }

                fn visit_seq<A>(self, mut seq: A) -> Result<[u8; 32], A::Error>
                    where
                        A: SeqAccess<'de>,
                    {
                        let mut arr = [0u8; 32];
                        for i in 0..32 {
                            arr[i] = seq
                                .next_element()?
                                .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                        }
                        Ok(arr)
                    }
            }

            let sha256: [u8; 32] = deserializer.deserialize_seq(Sha256Visitor)?;

            // try to lookup the sha256
            let meta = db::lookup_sha256(&sha256)
                .ok_or_else(|| Error::custom("unknown BIOS checksum"))?;

            // Create an "empty" BIOS instance, only referencing the
            // metadata. It's up to the caller to fill the blanks.
            let mut bios = Bios::dummy();

            bios.metadata = meta;

            Ok(bios)
        } 
}


/// Dummy metadata used as a placeholder for dummy BIOS instances
static DUMMY_METADATA: Metadata =
    Metadata {
        sha256: [0xff; 32],
        version_major: 0,
        version_minor: 0,
        region: Region::NorthAmerica,
        known_bad: true,
        animation_jump_hook: None,
        patch_debug_uart: None,
    };

/// BIOS images are always 512KB in length
pub const BIOS_SIZE: usize = 512 * 1024;
