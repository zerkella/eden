# Copyright (c) Facebook, Inc. and its affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import absolute_import

from testutil.dott import feature, sh, testtmp  # noqa: F401


sh % "cat" << r"""
[ui]
ssh = python "$TESTDIR/dummyssh"
[extensions]
commitcloud =
infinitepush =
[commitcloud]
hostname = testhost
servicetype = local
servicelocation = $TESTTMP
""" >> "$HGRCPATH"

sh % "setconfig 'remotefilelog.reponame=server'"
sh % "hg init server"
sh % "cd server"
sh % "cat" << r"""
[infinitepush]
server = yes
indextype = disk
storetype = disk
reponame = testrepo
""" >> ".hg/hgrc"

sh % "hg clone 'ssh://user@dummy/server' client -q"
sh % "cd client"


sh % "cat" << r"""
{ "workspaces_data" : { "workspaces": [ { "name": "user/test/old", "archived": true, "version": 0 }, { "name": "user/test/default", "archived": false, "version": 0 }  ] } }
""" >> "$TESTTMP/workspacesdata"

sh % "hg cloud list" == r"""
commitcloud: searching workspaces for the 'server' repo
Workspaces:
        default
run `hg cloud sl -w <workspace name>` to view the commits
run `hg cloud join -w <workspace name> --switch` to switch to a different workspace
"""

sh % "hg cloud list --all" == r"""
commitcloud: searching workspaces for the 'server' repo
Workspaces:
        old (archived)
        default
run `hg cloud sl -w <workspace name>` to view the commits
run `hg cloud join -w <workspace name> --switch` to switch to a different workspace
"""
