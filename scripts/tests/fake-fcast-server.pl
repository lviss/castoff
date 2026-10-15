#!/usr/bin/env perl
# A throwaway stand-in for the daemon's FCast TCP port, used only by
# scripts/tests/test_cast_sh.sh to drive cast.sh end to end without a real
# daemon. Binds an ephemeral port (so parallel test runs never collide),
# writes it to --port-file so the test script can read it back, accepts
# exactly one connection, reads (and discards) the one request frame a real
# daemon would also have consumed, then plays back --scenario's scripted
# reply frames.
use strict;
use warnings;
use IO::Socket::INET;
use Getopt::Long qw(GetOptions);
use FindBin;
require "$FindBin::Bin/../lib/fcast-client.pl";

my ( $port_file, $scenario );
GetOptions(
    'port-file=s' => \$port_file,
    'scenario=s'  => \$scenario,
) or die "usage: fake-fcast-server.pl --port-file PATH --scenario NAME\n";
die "--port-file and --scenario are required\n" unless $port_file && $scenario;

my $listener = IO::Socket::INET->new(
    LocalAddr => '127.0.0.1',
    LocalPort => 0,
    Proto     => 'tcp',
    Listen    => 1,
    ReuseAddr => 1,
) or die "listen failed: $!\n";

open( my $pf, '>', $port_file ) or die "write $port_file failed: $!\n";
print {$pf} $listener->sockport;
close($pf);

my $client = $listener->accept or die "accept failed: $!\n";
binmode($client);
my $request = read_frame($client);    # the one request a real daemon would consume.

if ( $scenario eq 'ping' ) {
    write_frame( $client, 13, '' );    # Pong
}
elsif ( $scenario eq 'echo-request' ) {
    # Sends the request body straight back (as a Pong body), so tests can
    # assert on exactly what cast.sh put on the wire -- e.g. that a Play's
    # --header flags became the body's `headers` object.
    write_frame( $client, 13, $request->{body} );
}
elsif ( $scenario eq 'queue-skip' ) {
    # Mirrors ClearQueue's real daemon behavior: a PlaybackUpdate is
    # published first (stopping playback), then the QueueState reply --
    # exactly the interleaving cast.sh's queue commands must read past.
    write_frame( $client, 6,  '{"generationTime":1,"state":0}' );
    write_frame( $client, 15, '{"generationTime":2,"items":[],"currentIndex":null}' );
}
elsif ( $scenario eq 'closes-without-reply' ) {
    # deliberately nothing written -- exercises the client's EOF handling.
}
else {
    die "unknown scenario: $scenario\n";
}

$client->close;
$listener->close;
