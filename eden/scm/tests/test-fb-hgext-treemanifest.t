#chg-compatible

  $ . "$TESTDIR/library.sh"


  $ hginit master
  $ cd master
  $ cat >> .hg/hgrc <<EOF
  > [remotefilelog]
  > server=True
  > [treemanifest]
  > server=True
  > EOF
  $ echo x > x
  $ hg commit -qAm 'add x'
  $ cd ..

  $ hgcloneshallow ssh://user@dummy/master client -q --noupdate
  $ cd client

Test autocreatetrees
  $ cat >> .hg/hgrc <<EOF
  > [treemanifest]
  > autocreatetrees=True
  > EOF
  $ cd ../master
  $ mkdir subdir
  $ echo z >> subdir/z
  $ hg commit -qAm 'add subdir/z'

  $ cd ../client
  $ hg pull
  pulling from ssh://user@dummy/master
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  added 1 changesets with 0 changes to 0 files
  $ hg up -r tip
  fetching tree '' 70f2c6726cec346b70b4f2ea65d0e2b9e1092a66, found via e4d61696a942
  2 trees fetched over * (glob)
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  2 files fetched over 1 fetches - (2 misses, 0.00% hit ratio) over * (glob) (?)
  $ ls_l $CACHEDIR/master/packs/manifests
  -r--r--r--    1114 cc7c46997bf8e05307412ce3ac5e2a1ccb159a11.dataidx
  -r--r--r--     262 cc7c46997bf8e05307412ce3ac5e2a1ccb159a11.datapack
  -r--r--r--    1196 f102ab2df8567126527b595bd4f2b2125cf98975.histidx
  -r--r--r--     183 f102ab2df8567126527b595bd4f2b2125cf98975.histpack

  $ hg debugdatapack $CACHEDIR/master/packs/manifests/*.dataidx
  $TESTTMP/hgcache/master/packs/manifests/cc7c46997bf8e05307412ce3ac5e2a1ccb159a11:
  subdir:
  Node          Delta Base    Delta Length  Blob Size
  ddb35f099a64  000000000000  43            (missing)
  
  (empty name):
  Node          Delta Base    Delta Length  Blob Size
  70f2c6726cec  000000000000  92            (missing)
  

Test that commit creates local trees
  $ hg up -q tip
  $ echo z >> subdir/z
  $ hg commit -qAm 'modify subdir/z'
  $ ls_l .hg/store/packs/manifests
  -r--r--r--    1114 4c1ed7b5ede2ef728698d7c27d195f42c74cd238.dataidx
  -r--r--r--     262 4c1ed7b5ede2ef728698d7c27d195f42c74cd238.datapack
  -r--r--r--    1196 7c930adf1e81d971be4609f898b1c6948cd50023.histidx
  -r--r--r--     183 7c930adf1e81d971be4609f898b1c6948cd50023.histpack
  $ hg debughistorypack .hg/store/packs/manifests/7c930adf1e81d971be4609f898b1c6948cd50023.histpack
  
  
  Node          P1 Node       P2 Node       Link Node     Copy From
  7a911436836f  70f2c6726cec  000000000000  7dd80139a0c9  
  
  subdir
  Node          P1 Node       P2 Node       Link Node     Copy From
  ac728a786423  ddb35f099a64  000000000000  7dd80139a0c9  
  $ hg debugdatapack .hg/store/packs/manifests/*.dataidx
  .hg/store/packs/manifests/4c1ed7b5ede2ef728698d7c27d195f42c74cd238:
  subdir:
  Node          Delta Base    Delta Length  Blob Size
  ac728a786423  000000000000  43            (missing)
  
  (empty name):
  Node          Delta Base    Delta Length  Blob Size
  7a911436836f  000000000000  92            (missing)
  

Test that manifest matchers work
  $ hg status --rev 1 --rev 2 -I subdir/a
  $ hg status --rev 1 --rev 2 -I subdir/z
  M subdir/z

Test rebasing a stack of commits results in a pack with all the trees

  $ echo >> subdir/y
  $ hg commit -qAm 'modify subdir/y'
  $ echo >> subdir/y
  $ hg commit -Am 'modify subdir/y again'
  $ hg rebase -d 085784c01c08984ae3b6f4e4a6e553035d58380b -s '.^'
  rebasing 6a2476258ba5 "modify subdir/y"
  fetching tree '' bc0c2c938b929f98b1c31a8c5994396ebb096bf0
  1 trees fetched over * (glob)
  rebasing f096b21e165f "modify subdir/y again"
  $ hg log -r '.^::.' -T '{manifest}\n'
  0d05c20bb7eb53dbfe91f834ed3f0c26ca6ca655
  8289b85c6a307a5a64ffe3bd80bd7998775c787a
  $ hg debugdatapack .hg/store/packs/manifests/*.datapack
  .hg/store/packs/manifests/3f3f675f03d1d5c32ce32a7ca749309fb59f4c9e:
  subdir:
  Node          Delta Base    Delta Length  Blob Size
  66460700b3a5  000000000000  86            (missing)
  
  (empty name):
  Node          Delta Base    Delta Length  Blob Size
  ff75b8ba8d79  000000000000  92            (missing)
  
  .hg/store/packs/manifests/4c1ed7b5ede2ef728698d7c27d195f42c74cd238:
  subdir:
  Node          Delta Base    Delta Length  Blob Size
  ac728a786423  000000000000  43            (missing)
  
  (empty name):
  Node          Delta Base    Delta Length  Blob Size
  7a911436836f  000000000000  92            (missing)
  
  .hg/store/packs/manifests/b68f34f2ea3d8cf08d4504d2c8a43775645f1c1a:
  subdir:
  Node          Delta Base    Delta Length  Blob Size
  a4e2f032ee0f  000000000000  43            (missing)
  
  (empty name):
  Node          Delta Base    Delta Length  Blob Size
  0d05c20bb7eb  000000000000  92            (missing)
  
  .hg/store/packs/manifests/c550c1ee0db73784322bd380c41efc1ee2db5d0e:
  subdir:
  Node          Delta Base    Delta Length  Blob Size
  ad0a48a2ec1e  000000000000  43            (missing)
  
  (empty name):
  Node          Delta Base    Delta Length  Blob Size
  8289b85c6a30  000000000000  92            (missing)
  
  .hg/store/packs/manifests/dcda72bf149ff3d15689406ee73e30c31a303630:
  subdir:
  Node          Delta Base    Delta Length  Blob Size
  d67ded07f949  000000000000  86            (missing)
  
  (empty name):
  Node          Delta Base    Delta Length  Blob Size
  8e5245404428  000000000000  92            (missing)
  

Test treemanifest with sparse enabled
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > sparse=
  > reset=
  > EOF
  $ hg sparse -I subdir
  $ hg reset '.^'
  1 changeset hidden
  $ hg status
  M subdir/y
  $ hg up -C .
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ hg sparse --reset

Test rebase two commits with same changes
  $ echo >> subdir/y
  $ hg commit -qm 'modify subdir/y #1'
  $ hg up -q '.^'
  $ echo >> x
  $ echo >> subdir/y
  $ hg commit -qm 'modify subdir/y #2'
  $ hg up -q '.^'
  $ echo >> noop
  $ hg add noop
  $ hg commit -Am 'rebase destination'
  $ hg rebase -d 'desc(rebase)' -r 6052526a0d67 -r 79a69a1547d7 --config rebase.singletransaction=True
  rebasing 6052526a0d67 "modify subdir/y #1"
  rebasing 79a69a1547d7 "modify subdir/y #2"
  $ hg debughistorypack .hg/store/packs/manifests/387394c1cfba657cf7ac620d361317dd969a5c70.histidx .hg/store/packs/manifests/3b9ccdeefd4d12bf729e949ffdd58c25525a53e2.histidx
  
  
  Node          P1 Node       P2 Node       Link Node     Copy From
  5ca06dca517c  8011431de863  000000000000  36098647e229  
  
  subdir
  Node          P1 Node       P2 Node       Link Node     Copy From
  ad0a48a2ec1e  a4e2f032ee0f  000000000000  36098647e229  
  
  
  Node          P1 Node       P2 Node       Link Node     Copy From
  8026e03c5a35  8011431de863  000000000000  904f99ba5a9e  
  
  subdir
  Node          P1 Node       P2 Node       Link Node     Copy From
  ad0a48a2ec1e  a4e2f032ee0f  000000000000  904f99ba5a9e  
