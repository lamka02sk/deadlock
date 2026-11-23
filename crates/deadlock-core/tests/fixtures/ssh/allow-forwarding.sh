#!/usr/bin/with-contenv bash
# Test server only: the image turns TCP forwarding off, and the wrong-credential tests
# would trip OpenSSH's per-source penalties for the tests after them.
sed -i 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' /config/sshd/sshd_config
grep -q '^PerSourcePenalties' /config/sshd/sshd_config || echo 'PerSourcePenalties no' >> /config/sshd/sshd_config
