  $ setconfig format.use-symlink-atomic-write=1

  $ mv $TESTTMP/hgcache/master/indexedlogdatastore/latest{,.bak}
  $ ln -s foo $TESTTMP/hgcache/master/indexedlogdatastore/latest || echo foo > $TESTTMP/hgcache/master/indexedlogdatastore/latest
  $ echo y > $TESTTMP/hgcache/master/indexedlogdatastore/0/index2-node
  $ rm .hg/store/metalog/roots/meta
  $ ln -s foo .hg/store/metalog/roots/meta || echo foo > rm .hg/store/metalog/roots/meta