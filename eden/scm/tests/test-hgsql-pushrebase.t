#chg-compatible

  $ . "$TESTDIR/hgsql/library.sh"
  $ disable treemanifest

  $ enable pushrebase
  $ setconfig experimental.bundle2lazylocking=True

Test verify sql lock is not held during prelockrebase and txnclose hooks

  $ cat >> $TESTTMP/locktester.py <<EOF
  > import os
  > from edenscm.mercurial import extensions, bundle2, util
  > def checklock(repo, *args, **kwargs):
  >     if len(repo.heldlocks) > 0:
  >         raise util.Abort("lock was TAKEN")
  >     raise util.Abort("lock was FREE")
  > EOF

  $ initserver master master
  $ cat >> master/.hg/hgrc <<EOF
  > [hooks]
  > prepushrebase=python:$TESTTMP/locktester.py:checklock
  > txnclose=python:$TESTTMP/locktester.py:checklock
  > EOF
  $ cd master
  $ touch a && hg ci -Aqm a
  error: txnclose hook failed: lock was FREE
  (run with --traceback for stack trace)
  $ hg book master
  error: txnclose hook failed: lock was FREE
  (run with --traceback for stack trace)
  $ cd ..

  $ initclient client
  $ cd client
  $ hg pull -q ssh://user@dummy/master
  $ hg up -q master
  $ touch b && hg ci -Aqm b

  $ hg push ssh://user@dummy/master --to master
  pushing to ssh://user@dummy/master
  searching for changes
  remote: lock was FREE
  abort: push failed on remote
  remote: error: prepushrebase hook failed: lock was FREE
  [255]

  $ cd ../master
  $ hg log -T '{rev}\n'
  0
