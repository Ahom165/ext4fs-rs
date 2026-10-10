//! Erreurs du moteur ext4.

use core::fmt;

/// Résultat typé du crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Toutes les erreurs retournées par `ext4-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Erreur de l'appareil bloc sous-jacent (code libre, ex. NTSTATUS ou errno).
    Device(i32),
    /// Fin de fichier / bloc hors limites.
    OutOfBounds { offset: u64, len: usize },
    /// Magic du superblock invalide (pas un volume ext4).
    BadMagic,
    /// Fonctionnalité ext4 incompatible et indispensable au montage.
    UnsupportedFeature(&'static str),
    /// Structure corrompue (générique).
    Corrupt(&'static str),
    /// Inode introuvable.
    InodeNotFound(u32),
    /// Entrée de répertoire introuvable.
    NotFound,
    /// Le nom existe déjà.
    AlreadyExists,
    /// Répertoire non vide (rmdir refusé).
    DirectoryNotEmpty,
    /// Chemin invalide.
    BadPath,
    /// Plus d'inodes libres.
    NoInodes,
    /// Plus de blocs libres.
    NoSpace,
    /// Le volume est monté en lecture seule.
    ReadOnly,
    /// Le volume/chemin est chiffré (fscrypt) : non pris en charge, clé absente.
    Encrypted,
    /// Journal jbd2 illisible ou incohérent.
    JournalCorrupt(&'static str),
    /// Journal externe (device séparé) : non pris en charge.
    ExternalJournal,
    /// Orphelins à nettoyer : exécuter `e2fsck` sur la machine Linux d'abord.
    OrphansPending,
    /// Arbre d'extents trop complexe pour l'opération demandée (feuille pleine, etc.).
    ExtentFull,
    /// Feuille HTree pleine : insertion refusée (limitation v1, voir README).
    DirIndexFull,
    /// Vérification de checksum échouée (bloc/inode/descripteur).
    ChecksumMismatch(&'static str),
    /// Taille demandée hors limites pour l'opération.
    InvalidSize,
    /// Déplacement d'un répertoire dans son propre sous-arbre.
    DirLoop,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Device(c) => write!(f, "erreur appareil bloc (code {c})"),
            Error::OutOfBounds { offset, len } => {
                write!(f, "accès hors limites : offset={offset}, len={len}")
            }
            Error::BadMagic => write!(f, "magic ext4 invalide (0xEF53 attendu)"),
            Error::UnsupportedFeature(s) => write!(f, "fonctionnalité non prise en charge : {s}"),
            Error::Corrupt(s) => write!(f, "structure corrompue : {s}"),
            Error::InodeNotFound(n) => write!(f, "inode {n} introuvable"),
            Error::NotFound => write!(f, "introuvable"),
            Error::AlreadyExists => write!(f, "existe déjà"),
            Error::DirectoryNotEmpty => write!(f, "répertoire non vide"),
            Error::BadPath => write!(f, "chemin invalide"),
            Error::NoInodes => write!(f, "plus d'inodes libres"),
            Error::NoSpace => write!(f, "plus de blocs libres"),
            Error::ReadOnly => write!(f, "volume en lecture seule"),
            Error::Encrypted => write!(f, "répertoire chiffré (fscrypt) non pris en charge"),
            Error::JournalCorrupt(s) => write!(f, "journal jbd2 corrompu : {s}"),
            Error::ExternalJournal => write!(f, "journal externe non pris en charge"),
            Error::OrphansPending => {
                write!(f, "orphelins en attente : lancer e2fsck avant montage")
            }
            Error::ExtentFull => write!(f, "arbre d'extents plein"),
            Error::DirIndexFull => write!(f, "feuille HTree pleine (limitation)"),
            Error::ChecksumMismatch(s) => write!(f, "checksum invalide : {s}"),
            Error::InvalidSize => write!(f, "taille invalide"),
            Error::DirLoop => write!(f, "déplacement d'un répertoire dans son propre sous-arbre"),
        }
    }
}

impl core::error::Error for Error {}
