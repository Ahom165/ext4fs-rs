# ext4fs-rs — driver ext4 natif pour Windows, en Rust

Pilote Windows **100 % Rust** (wdk-rs) qui rend l'ext4 accessible à Windows,
architecture **hybride** :

- **`ext4-core`** — moteur ext4 pur Rust (`no_std` + `alloc`), lecture **ET**
  écriture : superblock, GDT 32/64 octets, checksums `metadata_csum`, inodes,
  arbres d'extents, répertoires linéaires + **HTree**, `inline_data`, journal
  **jbd2** (replay + transactions écrites, checksums CRC32C).
- **`ext4drv`** — pilote kernel **WDM** (`wdk-rs`) embarquant `ext4-core`
  tel quel, avec **deux visages** :
  - **FSD (phase 2, actif par défaut)** : device système de fichiers
    enregistré (`IoRegisterFileSystem`). Les volumes ext4 sont montés par
    l'I/O manager, apparaissent avec une **lettre de lecteur dans
    l'Explorateur**, et tout fonctionne nativement : ouverture, lecture,
    écriture, copie, renommage, suppression, propriétés, auto-refresh.
  - **Device de contrôle `\\.\Ext4Fs`** (phase 1, conservé) : IOCTLs pour
    explorer des **images fichiers** ext4 depuis le kernel.
- **`ext4ctl`** — outil Windows user-mode : commandes phase 1 (images) +
  commandes FSD (`fsd-mount`, `fsd-unmount`, `fsd-status`) + **formatage
  ext4 d'un volume brut** (`format`, phase 3).
- **`ext4cli`** — CLI multiplateforme (Linux/Windows) sur image fichier :
  même moteur, idéale pour tester sans kernel (info/list/tree/cat/extract/
  write/mkdir/rm/link/symlink/mv/touch + EAs : ea_list/ea_get/ea_set/ea_rm).

> **Phase 2 livrée** : la couche FSD est implémentée (montage → lettre de
> lecteur, Cache Manager, IRP_MJ_CREATE/READ/WRITE/DIRECTORY_CONTROL/
> QUERY+SET_INFO/FSCTL/CLEANUP/CLOSE/FLUSH/LOCK, reparse symlinks,
> notifications, renommage e2fsck-validé). Voir docs/DESIGN.md §8-15.
>
> **v2 moteur** : attributs étendus (xattrs) complets — namespaces
> `user.`/`security.`/`trusted.` (+6 autres), blocs EA + corps d'inode,
> checksums et placement **calibrés octet par octet contre debugfs 1.47.2**
> (aller-retour moteur écrit → debugfs relit, e2fsck propre).
>
> **v2 FSD** : EAs Windows (IRP_MJ_QUERY_EA/SET_EA), flux alternatifs
> `fichier:flux` (≤ ~4 Ko, FILE_STREAM_INFORMATION), descripteurs de
> sécurité (QUERY/SET_SECURITY, défaut Everyone:full), oplocks FsRtl
> (break à l'écriture/cleanup) et worker de commit périodique (5 s).

---

## 1. Ce qui est prouvé (tests automatisés)

`ext4-core` est validé contre de **vraies images** créées par `mkfs.ext4`
(e2fsprogs 1.47.2) — **33 tests d'intégration + 7 unitaires + 5 tests
mkfs (45 au total)** :

| Domaine | Vérification |
|---|---|
| Lecture | contenu binaire identique (fichiers 1 o → 600 ko, 1K & 4K blocs, extents multi-niveaux, UTF-8, symlinks, hardlinks) |
| Écriture | création/écriture/append/overwrite/truncate/mkdir/rmdir/unlink/link → **`e2fsck -fn` propre** à chaque fois |
| Compteurs | free blocks/inodes sb+GDT+bitmaps strictement conformes à `dumpe2fs` |
| HTree | répertoires 8000 fichiers **réindexés par e2fsck -fD** (arbre créé par e2fsprogs), lookup + insertion → e2fsck valide |
| Journal | écriture + **crash simulé** → replay **maison** == replay **e2fsck** (l'implémentation noyau), multi-transactions (600 ko), checksums v3 |
| inline_data | fichiers ≤ 60 o inline (lisibles par debugfs), conversion automatique en extents au-delà |
| xattrs (EAs) | set/remove/list/get `user.`/`security.`/`trusted.` : bloc EA + corps d'inode, csum variante debugfs, e_hash exact (0xdf1d5a5b validé), aller-retour **moteur écrit → debugfs relit → e2fsck -fn propre** |
| mkfs (formatage) | image produite par le moteur : **e2fsck -fn propre** (4K journalisé, 1K sans journal, 256 MiB multi-groupes + backup sb/GDT), roundtrip debugfs 70 ko, remontage par le moteur, label/réservé honorés, refus volume trop petit |
| Refus propres | volumes chiffrés (fscrypt), BIGALLOC/MMP/META_BG, magic invalide, montage RO |

Lancer les tests (Linux, e2fsprogs requis) :

```bash
cargo test -p ext4-core                      # unitaires
cargo test -p ext4-core --features std-tests # intégration (mkfs.ext4 requis)
```

## 2. Build sous Windows

Prérequis :
- Visual Studio 2022 + **WDK** (10.0.26100+ ; pour les builds Insider Dev,
  installer le **WDK Insider Preview** correspondant à ton build),
- Rust stable ≥ 1.91 (`rustup update stable`),
- LLVM/libclang pour bindgen : `winget install -i LLVM.LLVM` (ajouter au PATH),
- `cargo install --locked cargo-make` (packaging INF2CAT si besoin),
- PowerShell admin, **test mode actif** (`bcdedit /set testsigning on` — déjà
  ton cas).

> ⚠️ Ton build est un **Insider Dev (29683.1000)** : les binaires kernel sont
> sensibles à la version. Si `cargo build` échoue au link, utilise l'eWDK
> (Enterprise WDK) correspondant à ton Insider build — c'est le chemin
> officiel Microsoft pour pilotes Insider.

```powershell
cd ext4fs-rs\scripts
.\build.ps1          # → dist\ext4drv.sys + dist\ext4ctl.exe
.\sign.ps1           # certificat de test auto-signé + signature Authenticode
.\install.ps1 -Start # copie + sc create ext4fs + démarrage
```

Désinstallation : `.\scripts\uninstall.ps1`.

## 3. Utilisation (phase 1 — contrôle par IOCTL)

```powershell
# monter une partition ext4 (lettre de disque Windows) en lecture seule
.\dist\ext4ctl.exe mount \\.\D: ro        # → volume id=1

# ou une image fichier (le chemin est préfixé \??\ automatiquement)
.\dist\ext4ctl.exe mount C:\images\ext4.img

# naviguer
.\dist\ext4ctl.exe info 1
.\dist\ext4ctl.exe list 1 /
.\dist\ext4ctl.exe list 1 /home/user
.\dist\ext4ctl.exe cat   1 /home/user/fichier.txt
.\dist\ext4ctl.exe extract 1 /home/user/archive.tar C:\temp\archive.tar

# écrire (sans "ro" au mount)
.\dist\ext4ctl.exe write 1 /home/user/nouveau.txt C:\temp\nouveau.txt
.\dist\ext4ctl.exe mkdir 1 /home/user/docs
.\dist\ext4ctl.exe rm    1 /home/user/old.txt
.\dist\ext4ctl.exe umount 1               # flush + état "clean" dans le journal
```

Le démontage (`umount`) est important : il finalise le journal et remet le
superblock en état "clean" pour le prochain montage Linux.

### ext4cli (sans pilote — Linux et Windows)

```bash
ext4cli info   ext4.img
ext4cli list   ext4.img /docs
ext4cli cat    ext4.img /docs/hello.txt
ext4cli extract ext4.img /docs/data.bin out.bin
ext4cli write  ext4.img /nouveau.txt local.txt
ext4cli tree   ext4.img

# attributs étendus (EAs)
ext4cli ea_list ext4.img /docs/fichier.txt
ext4cli ea_get  ext4.img /docs/fichier.txt user.comment
ext4cli ea_set  ext4.img /docs/fichier.txt user.comment "mon texte"
ext4cli ea_rm   ext4.img /docs/fichier.txt user.comment
```

### FSD : volume ext4 dans l'Explorateur (phase 2)

Une fois le pilote installé et démarré (`install.ps1 -Start`), tout volume
ext4 présent sur une partition est automatiquement proposé au montage :

1. Branche le disque / ouvre la partition. Windows tente de monter le volume ;
   ext4drv vérifie le superblock et prend le volume.
2. Le mount manager attribue une lettre — le volume apparaît dans
   **l'Explorateur**, avec son label ext4.
3. Utilise tout comme un disque NTFS : copier/coller, ouvrir/enregistrer,
   renommer (F2), supprimer, créer des dossiers, suivre les liens
   symboliques. Les liens créés côté Windows sont des symlinks NT ; ceux
   créés côté Linux apparaissent avec leur cible POSIX (résolus si
   absolus sur ce volume).
4. Pour éjecter proprement : `ext4ctl fsd-unmount X:` (flush + journal
   finalisé) ou déconnexion matérielle sans risque.
5. Commandes utiles :

```powershell
ext4ctl fsd-mount \\.\HarddiskVolume5   # force le montage d'un volume
ext4ctl fsd-status                       # liste les volumes ext4 montés
ext4ctl fsd-unmount E:                   # démontage propre
```

Validation recommandée après une session d'écriture : sur la machine Linux,
`e2fsck -fn /dev/sdXN` doit être **propre** (le journal a été maintenu par le
pilote, replay compatible kernel).

### Formater un disque en ext4 depuis Windows (phase 3)

`ext4ctl format` écrit un ext4 complet sur un volume brut — purement
user-mode, même modèle que format.com (verrou exclusif + démontage, aucun
IOCTL pilote) :

```powershell
# formate E: en ext4 (4K, journal 4 MiB, confirmation en tapant la cible)
ext4ctl format \\.\E:

# options : label, taille de bloc, journal, ratio d'inodes, réservé, uuid
ext4ctl format \\.\E: --label Photos --4k --journal 16 --yes
ext4ctl format \\.\E: --no-journal --1k --ratio 8192 --reserved 1 --yes
ext4ctl format \\.\PhysicalDrive2 --label Backup --yes

# puis monte-le dans l'Explorateur
ext4ctl fsd-mount \\.\E:
```

Le volume produit passe `e2fsck -fn` côté Linux et se monte par le pilote
comme par n'importe quel Linux ext4. Bornes v1 : 1 MiB..16 TiB, admin
requis ; le verrou refusera le formatage si des fichiers sont ouverts ou
qu'une pagefile réside sur le volume.

## 4. Sécurité des données (à lire avant d'écrire)

- Le moteur écrit des transactions **jbd2 conformes** (validées par le replay
  du noyau Linux) et des checksums `metadata_csum` exacts — mais c'est un
  pilote **jeune** : garde une **sauvegarde** de ton disque/image avant tout
  montage en écriture.
- Le replay d'un journal non vide est appliqué **au montage** (comme Linux) ;
  un volume avec orphelins en attente est refusé (passe `e2fsck` côté Linux).
- Chiffrement (`fscrypt`) : accès refusé proprement (pas de corruption).
- En cas de doute : `e2fsck -fn <image>` te dit en 1 s si tout est cohérent.

## 5. Architecture du code

```
crates/
├── ext4-core/        moteur ext4 no_std (aucune dépendance externe)
│   ├── superblock.rs GDT, features, checksums
│   ├── gdt.rs        descripteurs 32/64 B + csums bitmaps
│   ├── inode.rs      inodes, checksums lo/hi, inline_data
│   ├── extent.rs     flatten + rebuild des arbres d'extents
│   ├── dir.rs        dirent, HTree, hash dx (half_md4/TEA), tails
│   ├── journal.rs    jbd2 : replay + transactions (v2/v3), wrap
│   ├── xattr.rs      EAs : blocs EA + ibody, csums calibrés e2fsprogs
│   ├── fs.rs         montage, cache blocs, allocateurs, transactions
│   └── fsops.rs      chemins, read/write, namespace
├── ext4drv/          pilote WDM (wdk-rs) — compile sous Windows uniquement
├── ext4ctl/          outil IOCTL Windows
└── ext4cli/          CLI multiplateforme
scripts/              build/sign/install PowerShell
docs/DESIGN.md        design complet (phase 1 + FSD phase 2 livrée)
```

### Pourquoi WDM et pas KMDF ?

Un FSD Windows est intrinsèquement WDM (VPB, Cache Manager, mount manager
n'existent pas en KMDF). Le pilote est donc WDM pur dès le départ — la VDO de
la phase 2 s'ajoutera au même DRIVER_OBJECT sans migration.

## 6. Dépannage

| Symptôme | Cause / remède |
|---|---|
| `sc start` → erreur 577 | pilote non signé ou test mode inactif → `sign.ps1`, `bcdedit /set testsigning on`, reboot |
| `ext4ctl` : "impossible d'ouvrir \\.\Ext4Fs" | service pas démarré → `sc start ext4fs` |
| montage échoué `0xC00000BB` | feature non supportée (BIGALLOC/MMP/fscrypt…), voir message DbgPrint (DebugView, Capture Kernel) |
| Insider Dev : link errors kernel | utiliser l'eWDK de la build Insider correspondante |
| `DirIndexFull` | feuille HTree pleine (limitation v1) : réorganiser ou attendre la phase 2 |
