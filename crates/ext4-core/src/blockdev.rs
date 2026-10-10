//! Abstraction appareil bloc : image fichier, disque physique, ou (côté
//! pilote) lecture/écriture via Zw* sur un fichier image ou un volume.

use crate::error::{Error, Result};

/// Périphérique bloc abstrait. Toutes les opérations sont des accès
/// positionnés en octets ; `ext4-core` n'accède jamais au-delà de `size()`.
pub trait BlockDevice {
    /// Flush des buffers de l'appareil (no-op par défaut; les devices
    /// noyau envoient un IRP_MJ_FLUSH_BUFFERS).
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// Lit exactement `buf.len()` octets à `offset`.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// Écrit exactement `buf.len()` octets à `offset`.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;
    /// Taille totale de l'appareil en octets.
    fn size(&mut self) -> Result<u64>;
    /// Appareil protégé en écriture (le moteur refusera le montage RW).
    fn is_read_only(&self) -> bool {
        false
    }
}

/// Vérifie que [offset, offset+len) tient dans `limit`.
#[inline]
pub fn check_range(offset: u64, len: usize, limit: u64) -> Result<()> {
    let end = offset
        .checked_add(len as u64)
        .ok_or(Error::OutOfBounds { offset, len })?;
    if end > limit {
        return Err(Error::OutOfBounds { offset, len });
    }
    Ok(())
}
