# Third-Party Notices

APFS Explorer for Windows builds upon prior work in open-source APFS
implementations and Windows user-mode filesystem infrastructure:

## Reference implementations

- linux-apfs-rw (Ernesto Fernández) - GPL-2.0+ - github.com/eafer/linux-apfs-rw
- libfsapfs (libyal) - LGPL-3.0+ - github.com/libyal/libfsapfs
- apfs-fuse (sgan81) - GPL-2.0 - github.com/sgan81/apfs-fuse
- apfsprogs (Ernesto Fernández) - GPL-2.0+ - github.com/eafer/apfsprogs
- go-apfs (blacktop) - MIT - github.com/blacktop/go-apfs

## Specification

- APFS specification - Apple's published file system reference (PDF, public)

## Runtime infrastructure

- WinFsp (Bill Zissimopoulos) - GPL-3.0+ with commercial exception - winfsp.dev

The Windows installer bundles and silently installs the **unmodified** official
WinFsp MSI when WinFsp is not already present. WinFsp is licensed under GPL-3.0
(© Bill Zissimopoulos); its complete source is available at
<https://github.com/winfsp/winfsp>.
