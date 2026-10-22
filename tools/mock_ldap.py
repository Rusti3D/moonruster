#!/usr/bin/env python3
# In-memory LDAP server (ldaptor) on 127.0.0.1:3389 for the ldap test.
# Run with a python that has ldaptor installed.
from io import BytesIO
from twisted.internet import reactor
from twisted.internet.protocol import ServerFactory
from twisted.python.components import registerAdapter
from ldaptor.inmemory import fromLDIFFile
from ldaptor.interfaces import IConnectedLDAPEntry
from ldaptor.protocols.ldap.ldapserver import LDAPServer

LDIF = b"""\
dn: dc=example,dc=org
dc: example
objectClass: dcObject

dn: ou=people,dc=example,dc=org
ou: people
objectClass: organizationalUnit

dn: uid=alice,ou=people,dc=example,dc=org
uid: alice
cn: Alice
objectClass: Person
userPassword: alicepw
memberOf: cn=Printers,ou=groups,dc=example,dc=org

dn: uid=bob,ou=people,dc=example,dc=org
uid: bob
cn: Bob
objectClass: Person
userPassword: bobpw
memberOf: cn=other,ou=groups,dc=example,dc=org

dn: uid=st*r,ou=people,dc=example,dc=org
uid: st*r
cn: Star
objectClass: Person
userPassword: starpw

"""

class LDAPServerFactory(ServerFactory):
    protocol = LDAPServer

    def __init__(self, root):
        self.root = root

    def buildProtocol(self, addr):
        proto = self.protocol()
        proto.debug = False
        proto.factory = self
        return proto

def start(db):
    registerAdapter(lambda x: x.root, LDAPServerFactory, IConnectedLDAPEntry)
    reactor.listenTCP(3389, LDAPServerFactory(db), interface="127.0.0.1")

d = fromLDIFFile(BytesIO(LDIF))
d.addCallback(start)
reactor.run()
