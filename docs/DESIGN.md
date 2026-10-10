# Design — ext4fs-rs : architecture et feuille de route vers l'Explorateur

## 1. Vue d'ensemble (architecture hybride livrée)

```
┌──────────────────────────────────────────────────────────────────────┐
│  Windows (x86_64, Win11 22H2–24H2, test signing)                     │
│                                                                      │
│  ext4ctl.exe ──DeviceIoControl──▶ \\.\Ext4Fs (ext4drv.sys, WDM/Rust) │
│        ▲                              │  DriverEntry → device        │
│        │                              │  IOCTL : mount/list/read/... │
│        │                              ▼                              │
│  ext4cli.exe                   ext4-core (no_std + alloc, Rust pur)  │
│  (image fichier,                      │  superblock/GDT/inodes       │
│   lecture/écriture)                   │  extents/HTree/journal jbd2  │
│                                       ▼                              │
│                              BlockDevice trait                       │
│                          ┌────────────┴─────────────┐                │
│                   FileDevice (std)            ZwBlockDevice (kernel) │
│                   image fichier               ZwCreateFile/ZwReadFile│
└──────────────────────────────────────────────────────────────────────┘
```

Le moteur `ext4-core` est **le même code binaire de logique** quel que soit
l'hôte : testé et validé sous Linux (27 tests, juge de paix `e2fsck`), il est
embarqué tel quel dans le pilote kernel.

### Cohérence des écritures (protocol de commit)

Chaque opération haut niveau (write_file, create, unlink…) suit :

1. mutations dans un cache bloc (bitmaps, tables d'inodes, GDT, sb, données) ;
2. `journal.begin()` : arme `s_start` du superblock journal (un crash pendant
   l'écriture laisse le replay s'arrêter proprement) ;
3. `journal.write_txn()` : transaction jbd2 complète (descriptor + données
   échappées + commit, checksums CSUM v2/v3) écrite **directement** ;
4. flush du cache vers l'appareil (checkpoint immédiat) ;
5. `journal.finalize()` : `s_start = 0` (log vide).

Un crash entre 3 et 5 est sûr : le replay applique la transaction ; après 5,
le journal est vide et les données déjà en place. Les transactions plus
grosses que le journal sont découpées en chunks (`MAX_TXN_CHUNK`).

## 2. Limitations v1 (documentées, non bloquantes)

- Répertoires HTree **existants** : lecture + insertion en feuille ok ; si la
  feuille est pleine, l'insertion est refusée (`DirIndexFull`). Les
  répertoires **créés** par le moteur sont linéaires (toujours insérables).
- `e2fsck -fn` signale les sauvegardes de GDT (sparse backups) non mises à
  jour — le FS reste montable ; e2fsck resynchronise.
- `fscrypt` (répertoires chiffrés) : détecté, refusé proprement (`Encrypted`).
  Une v2 pourrait décrypter avec une clé fournie (AES-256-XTS/CTS + HKDF).
- `BIGALLOC` (clusters), `META_BG`, MMP, fast commits, journal externe :
  refusés au montage.
- Symlink longs (> 59 o) : refusés (les rapides suffisent à ~99 % des cas).

## 3. Phase 2 — FSD : exposer l'ext4 dans l'Explorateur

L'étape qui manque pour voir `E:\` dans l'Explorateur n'est pas le format
(le moteur est prêt) mais la **couche FSD** (File System Driver) Windows.
Blueprint :

### 3.1 Objets à créer

1. **Driver de contrôle** (livré) : orchestration, démontage propre.
2. **Device disque (VDO)** : pour chaque volume monté, `IoCreateDevice` avec
   `FILE_DEVICE_DISK_FILE_SYSTEM`, attaché au VPB du volume.
3. **Registre auprès du Mount Manager** :
   - `IoRegisterFileSystem(vdo)`,
   - répondre à `IRP_MN_MOUNT_VOLUME` (il reçoit le DeviceObject du disque ;
     lire le superblock à l'offset 1024, créer la VDO-VPB liaison),
   - `IoCreateDisk`/`IoSetDeviceToVerify`… via l'API mount manager
     (`FsRtlRegisterFileSystem` + `IoVolumeDeviceToDosName` pour la lettre).

### 3.2 Dispatch IRP à implémenter dans la VDO

| IRP                      | Rôle                                | Support ext4-core |
|--------------------------|-------------------------------------|-------------------|
| `IRP_MJ_CREATE`          | ouverture fichier (résolution chemin)| `resolve`/`lookup` |
| `IRP_MJ_READ`/`WRITE`    | E/S avec cache manager              | `read_file`/`write_file` |
| `IRP_MJ_QUERY_INFORMATION` | taille/timestamps/attributs       | `stat`            |
| `IRP_MJ_SET_INFORMATION` | rename/truncate/delete              | `set_len`, `unlink` |
| `IRP_MJ_DIRECTORY_CONTROL` | énumération (NtQueryDirectoryFile)| `read_dir`        |
| `IRP_MJ_QUERY_VOLUME_INFORMATION` | FS info                    | `volume_info`     |
| Fast I/O (`FAST_IO_DISPATCH`) | lectures cachenées rapides     | wrapper sur read  |

### 3.3 Intégrations kernel obligatoires

- **Cache Manager** : `CcInitializeCacheMap` par FileObject, `CcReadData`/
  `CcSetFileSizes`… ; les mappings mémoire (mmap des exécutables) exigent
  `CC_FILE_SIZING` + support `IRP_MN_MSI`… (implémenter `CcCanWrite`).
- **FsRtl** : `FsRtlNotifyVolumeEvent`, `FsRtlIsNtstatusExpected`,
  oplock/leases via `FsRtlCheckOplock` (Explorer s'y attend).
- **VPB** : `Vpb->ReferenceCount`, `RealDevice` — le montage
  `IRP_MN_MOUNT_VOLUME` fournit `IrpSp->Parameters.MountVolume.Vpb`.
- **Mount Manager** : `IOCTL_MOUNTMGR_VOLUME_ARRIVAL_NOTIFICATION`
  pour obtenir une lettre de lecteur automatique.

### 3.4 Plan de développement par tranches

| Étape | Livrable | Vérification |
|-------|----------|--------------|
| 1 | VDO + `IRP_MN_MOUNT_VOLUME` sur un `.vhd` test | `mountvol`, DebugView |
| 2 | CREATE/CLOSE + QUERY_INFO + DIRECTORY_CONTROL | `dir E:\`, Explorer |
| 3 | READ via Cache Manager + Fast I/O | ouverture/copie de fichiers |
| 4 | WRITE + SET_INFORMATION (truncate/delete/rename) | édition depuis l'Explorer |
| 5 | lettres de lecteur via Mount Manager + notifications | montage auto |
| 6 | oplocks + enfin écriture jbd2 fine-grained (per-inode) | stress tests |

Le moteur actuel couvre les points 1 à 4 côté logique ; le travail restant
est l'intégration des mécanismes Windows (Cc/FsRtl/mount manager) —
majoritairement du câblage WDM standard, sans réécriture du format.

## 4. Décision de conception : WDM plutôt que KMDF

Un FSD est intrinsèquement WDM (`IoRegisterFileSystem`, VPB, Cc, mount
manager n'existent pas côté KMDF). Le device de contrôle est donc écrit en
WDM pur via `wdk-sys` — la même base que la future VDO, pour éviter une
migration KMDF→WDM. KMDF reste utilisable pour les filtres annexes (décrit
dans le README).

---

# Phase 2 — FSD implémentée (volume natif dans l'Explorateur)

## 8. Architecture de la couche FSD (livrée dans `crates/ext4drv/src/`)

```
                      ┌─────────────────────────────────────────────────┐
   Explorateur /      │  ext4drv.sys (WDM/Rust)                         │
   apps Win32         │                                                 │
        │             │  ┌───────────────┐      ┌────────────────────┐  │
   \\.\E:\fichier     │  │ device FS     │◀─────│ device volume      │  │
        ▼             │  │ (sans nom)    │ mount│ (créé au montage)  │  │
   IopParseDevice     │  │ IoRegister-   │─────▶│ VPB → Vcb          │  │
        │             │  │ FileSystem    │      │ FsContext = Fcb    │  │
        ▼             │  └───────────────┘      │ FsContext2 = Ccb   │  │
  IRP_MJ_* (Win32)    │                         └─────────┬──────────┘  │
        ▼             │  fsd.rs        fsd_create.rs      │             │
  fsd_dispatch ───────┼─▶ mount/VCB    CREATE/reparse     │             │
        │             │  fsd_io.rs     fsd_info.rs        ▼             │
        │             │  READ/WRITE    QUERY/SET_INFO     │             │
        │             │  fsd_fsctl.rs (CLEANUP/FSCTL)     │             │
        │             │        │ Cache Manager (Cc)       │             │
        │             │        ▼                          ▼             │
        │             │  ext4-core : read_file/write_file/rename/…      │
        │             │        │                                        │
        │             │        ▼ DevBlockDevice                         │
        │             │  stream file object + IRP_MJ_READ/WRITE/FLUSH   │
        │             │        ▼                                        │
        │             │  pile disque (volmgr → volsnap → disk.sys)      │
        └─────────────┼─────────────────────────────────────────────────┘
```

## 9. Cycle de montage (lettre de lecteur)

1. Un composant Windows ouvre un volume (`\\.\X:`) → l'I/O manager envoie
   `IRP_MN_MOUNT_VOLUME` au device FS (via `IoRegisterFileSystem`).
2. Le pilote crée un **stream file object** sur le device réel
   (`IoCreateStreamFileObject(NULL, Vpb->RealDevice)`) — jamais d'ouverture
   par chemin NT (récursion de montage impossible).
3. `ext4-core::Filesystem::mount` lit le superblock via des IRP_MJ_READ
   manuels (`DevBlockDevice`). Pas un ext4 → `STATUS_UNRECOGNIZED_VOLUME`,
   Windows essaiera le FS suivant (NTFS/FAT…).
4. `IoCreateDevice(FILE_DEVICE_DISK_FILE_SYSTEM)` = device volume + Vcb ;
   `Vpb->DeviceObject`, `SerialNumber`, `VolumeLabel` (s_volume_name).
5. Notification au **mount manager**
   (`IOCTL_MOUNTMGR_VOLUME_ARRIVAL_NOTIFICATION`, cible `\Device\HarddiskVolumeN`)
   → attribution automatique d'une lettre (base de données persistante, sinon
   première lettre libre). La lettre est récupérée via
   `IOCTL_MOUNTMGR_QUERY_DOS_VOLUME_PATH` et mémorisée dans le Vcb (cibles
   des liens symboliques).

## 10. Modèle de données FSD

| Objet | Contenu | Durée de vie |
|---|---|---|
| **Vcb** (DeviceExtension du volume) | `Filesystem` ext4-core, liste des FCBs, verrou FAST_MUTEX, lettre, label, notify_sync | montage → démontage |
| **Fcb** (par inode, cache `BTreeMap`) | taille/temps/attributs, SectionObjectPointers, FAST_MUTEX lazy-write, FILE_LOCK FsRtl, liste notify | volume monté (purge au démontage) |
| **Ccb** (par FILE_OBJECT) | type (volume/dir/fichier), chemin, curseur d'énumération, pattern, delete_pending | ouverture → close |

Sémantique de verrouillage : **un FAST_MUTEX par VCB** protège ext4-core ;
règle d'or — aucun appel `Cc*` sous le verrou (les paging writes reprennent
le verrou, jamais récursivement). Le verrou `lazy_mutex` par FCB sérialise
lazy-writer ↔ set_len (ordre lazy→VCB partout, pas d'inversion).

## 11. chemins des données

- **Lecture cache** : `CcCopyRead` (lazy init `CcInitializeCacheMap`).
- **Lecture paging/nocache** : direct `ext4-core::read_file` dans le buffer
  MDL (verrou VCB).
- **Écriture cache** : `CcCopyWrite` → `CcFlushCache` immédiat (write-through)
  → commit journal si dirty. Les paging writes s'écrivent via
  `ext4-core::write_file` (l'allocation d'extents se fait à la volée, bornée
  par la taille du fichier).
- **i_size ext4** = taille Windows : synchronisée à CLEANUP, FLUSH,
  SET_END_OF_FILE et dismount. Un crash brutal peut donc raccourcir les
  derniers fichiers non fermés (le contenu déjà flushe est sûr) — limitation
  v1 documentée.

## 12. Espace de noms

- **CREATE** : marche de chemin composant par composant (`lookup`), `.`, `..`,
  dispositions FILE_OPEN/CREATE/OPEN_IF/OVERWRITE(_IF)/SUPERSEDE,
  `SL_OPEN_TARGET_DIRECTORY` (rename), `FILE_DELETE_ON_CLOSE`.
- **Liens symboliques** : un symlink rencontré (hors
  `FILE_OPEN_REPARSE_POINT`) déclenche `STATUS_REPARSE` avec un chemin NT
  absolu reconstruit (lettre du volume ou `\\??\C:\…` pour les cibles Win32 ;
  cibles POSIX `/…` résolues sur ce volume, relatives par rapport au
  répertoire du lien). `FSCTL_GET_REPARSE_POINT` reconstruit le buffer
  `IO_REPARSE_TAG_SYMLINK`, `FSCTL_SET_REPARSE_POINT` réécrit la cible.
  Côté Linux, la cible du symlink apparaît comme une chaîne NT (lisible mais
  non résoluable) — compromis assumé.
- **QUERY_DIRECTORY** : snapshot du répertoire caché dans le CCB, classes
  Directory/Full/Both/Names/IdBoth/IdFull (layouts pack(4) x64), filtrage
  `FsRtlIsNameInExpression`, index/restart/single-entry.
- **Notifications** : `FsRtlNotifyFullChangeDirectory` + rapports
  add/remove/rename (rafraîchissement automatique de l'Explorateur).
- **Renommage** : `FileRenameInformation(Ex)` → `ext4-core::rename`
  (cross-directory, overwrite, '..' des répertoires — validé e2fsck).
- **Suppression** : `FileDispositionInformation(Ex)` pose `delete_pending` ;
  l'unlink/rmdir réel a lieu au CLEANUP de la dernière poignée
  (image section non mappée exigée via `MmFlushImageSection`).
- **Verrous de plage** : FsRtl `FILE_LOCK` (`FsRtlProcessFileLock`).

## 13. Sécurité / permissions (v1)

Les bits Unix ne sont pas modélisés : tous les fichiers sont accessibles
(lecture/écriture selon le volume). Les fichiers exécutables (mode x) sont
présentés READ-ONLY côté Windows pour éviter de casser un binaire ELF.
Pas de ACL/quotas (rejet propre).

Les **xattrs (EAs)** sont eux supportés par le moteur (v2) : namespaces
`user.`/`system.`/`security.`/`trusted.` (+5 autres, ordre e2fsprogs),
bloc EA externe et corps d'inode (ibody), checksums et placement calibrés
contre debugfs 1.47.2 — accessibles via `ext4cli ea_*`.

Côté FSD (v2), tout est câblé sur ce socle : IRP_MJ_QUERY_EA/SET_EA
(`user.EA$<nom>`), flux alternatifs `fichier:flux` (`user.ADS$<nom>`,
FILE_STREAM_INFORMATION inclus), descripteur de sécurité via
IRP_MJ_QUERY/SET_SECURITY (`user.WSD`, SD self-relative retourné brut,
défaut = Everyone contrôle total).

## 14. Plan de validation Windows (machine utilisateur)

1. `build.ps1` → `sign.ps1` (certificat test) → `install.ps1 -Start`.
2. Brancher un disque/partition ext4 (ou `fsd-mount \\.\HarddiskVolumeN`).
3. Vérifier la lettre dans l'Explorateur, parcourir, ouvrir un fichier,
   copier un fichier depuis/vers NTFS, créer/déplacer/supprimer, renommer.
4. `ext4ctl fsd-status` ; flush via `ext4ctl fsd-unmount X:`.
5. Sur la machine Linux de contrôle : `e2fsck -fn` du volume → doit être
   propre (le journal a été tenu à jour par le pilote).

## 15. Limitations v2 FSD (non exhaustif)

Câblé en v2 : IRP_MJ_QUERY_EA/SET_EA (stockage `user.EA$*`), flux
alternatifs `fichier:flux` (`user.ADS$*`, FILE_STREAM_INFORMATION,
QUERY/SET sur flux, DELETE au CLEANUP), descripteurs de sécurité
(IRP_MJ_QUERY/SET_SECURITY sur `user.WSD`, défaut Everyone:full),
oplocks FsRtl (requêtes LEVEL_1/2, BATCH, FILTER + acquittements ; break
à l'écriture et au CLEANUP), worker de commit périodique (thread
système, flush Cc + journal toutes les 5 s, arrêt propre au déchargement),
junctions/points de montage tolérés.

Reste (v3) :

- pas de break d'oplock à la réouverture (level 1 casse à l'écriture) ;
- pas d'ACL réelle (SeAccessCheck) : le SD stocké est retourné brut ;
- pas d'EA-inodes ni de quotas ;
- ADS ≤ ~1 bloc EA (~4 Ko) et sans section mappée (pas de Cc) ;
- renommage d'un flux alternatif non supporté (STATUS_UNSUCCESSFUL) ;
- metadata_csum + extents requis en écriture (règles phase 1) ;
- symlinks rapides ≤ 58 octets pour SET_REPARSE_POINT.
