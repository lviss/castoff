#!/usr/bin/env perl
# A throwaway stand-in for the daemon's image-upload HTTP endpoint
# (daemon/src/upload.rs's `POST /images`), used only by
# scripts/tests/test_cast_sh.sh to drive cast.sh's upload-image end to end.
# Binds an ephemeral port, writes it to --port-file, accepts exactly one
# HTTP request, discards it, and replies with a canned upload response.
use strict;
use warnings;
use IO::Socket::INET;
use Getopt::Long qw(GetOptions);

my $port_file;
GetOptions( 'port-file=s' => \$port_file )
    or die "usage: fake-http-server.pl --port-file PATH\n";
die "--port-file is required\n" unless $port_file;

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

my $content_length = 0;
while ( my $line = <$client> ) {
    $line =~ s/\r?\n$//;
    last if $line eq '';
    $content_length = $1 if $line =~ /^Content-Length:\s*(\d+)/i;
}
if ($content_length) {
    my $discard;
    read( $client, $discard, $content_length );
}

my $json = '{"id":"test-id-1","url":"file:///tmp/test-id-1.jpg","container":"image/jpeg"}';
print {$client} "HTTP/1.1 200 OK\r\n";
print {$client} "Content-Type: application/json\r\n";
print {$client} 'Content-Length: ' . length($json) . "\r\n";
print {$client} "Connection: close\r\n";
print {$client} "\r\n";
print {$client} $json;

$client->close;
$listener->close;
