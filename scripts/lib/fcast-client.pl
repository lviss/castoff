#!/usr/bin/env perl
# FCast v2 wire framing + a one-shot TCP round trip, shared by scripts/cast.sh
# (as a CLI, shelled out to per command) and scripts/tests/*.pl (as a library,
# `require`d for the pure-framing tests below `main`'s `unless caller` guard).
#
# Framing matches daemon/src/fcast.rs byte for byte: a 4-byte little-endian
# length prefix (opcode byte + body, so a body-less frame has length 1), one
# opcode byte, then an optional UTF-8 JSON body, max packet 32 KiB.
use strict;
use warnings;
use IO::Socket::INET;
use Getopt::Long qw(GetOptions);

our $MAX_PACKET_SIZE = 32 * 1024;

# castoff's own private queue extension (daemon/src/fcast.rs's Opcode enum):
# a queue command's reply isn't always the very next frame, since some queue
# commands (e.g. ClearQueue) also publish a PlaybackUpdate to every
# connection first. read_queue_reply below skips exactly this opcode.
our $OPCODE_PLAYBACK_UPDATE = 6;
our $OPCODE_QUEUE_STATE = 15;

sub encode_frame {
    my ( $opcode, $body ) = @_;
    $body = '' unless defined $body;
    my $len = 1 + length($body);
    die "frame too large: $len bytes\n" if $len > $MAX_PACKET_SIZE;
    return pack( 'V', $len ) . chr($opcode) . $body;
}

sub write_frame {
    my ( $fh, $opcode, $body ) = @_;
    print {$fh} encode_frame( $opcode, $body ) or die "write failed: $!\n";
}

# Reads exactly $n bytes from $fh. Returns undef only on a clean EOF before
# any byte of this read arrived; a partial read beyond that is a protocol
# error (the far end closed mid-frame), not a clean EOF.
sub read_exact {
    my ( $fh, $n ) = @_;
    return '' if $n == 0;
    my $buf = '';
    while ( length($buf) < $n ) {
        my $chunk;
        my $got = read( $fh, $chunk, $n - length($buf) );
        die "read failed: $!\n" unless defined $got;
        if ( $got == 0 ) {
            return undef if length($buf) == 0;
            die "unexpected end of stream\n";
        }
        $buf .= $chunk;
    }
    return $buf;
}

# Reads one frame from $fh. Returns `undef` on clean EOF before any new
# frame starts, matching daemon::fcast::read_frame's contract.
sub read_frame {
    my ($fh) = @_;
    my $len_bytes = read_exact( $fh, 4 );
    return undef unless defined $len_bytes;
    my $len = unpack( 'V', $len_bytes );
    if ( $len <= 0 || $len > $MAX_PACKET_SIZE ) {
        die "invalid FCast frame length $len\n";
    }
    my $payload = read_exact( $fh, $len );
    die "unexpected end of stream\n" unless defined $payload;
    my $opcode = ord( substr( $payload, 0, 1 ) );
    my $body = substr( $payload, 1 );
    return { opcode => $opcode, body => $body };
}

# Reads a single reply frame, whatever its opcode -- the generic case every
# non-queue command uses (mirrors the Android client's sendCommand, which
# does not validate the reply's opcode either).
sub read_reply {
    my ($fh) = @_;
    my $frame = read_frame($fh);
    die "connection closed before a reply arrived\n" unless defined $frame;
    return $frame;
}

# Reads frames until a QueueState arrives, discarding any interleaved
# PlaybackUpdate along the way -- see the module doc comment above and
# FCastClient.kt's readQueueStateReply (castoff-android) for the full
# reasoning this mirrors.
sub read_queue_reply {
    my ($fh) = @_;
    while (1) {
        my $frame = read_frame($fh);
        die "connection closed while awaiting a QueueState reply\n"
            unless defined $frame;
        next if $frame->{opcode} == $OPCODE_PLAYBACK_UPDATE;
        if ( $frame->{opcode} == $OPCODE_QUEUE_STATE ) {
            return $frame;
        }
        die "expected a QueueState reply, got opcode $frame->{opcode}\n";
    }
}

sub main {
    my ( $host, $port, $opcode, $body, $queue_reply );
    GetOptions(
        'host=s'      => \$host,
        'port=i'      => \$port,
        'opcode=i'    => \$opcode,
        'body=s'      => \$body,
        'queue-reply' => \$queue_reply,
    ) or die "usage: fcast-client.pl --host H --port P --opcode N [--body JSON] [--queue-reply]\n";
    die "--host, --port and --opcode are required\n"
        unless defined $host && defined $port && defined $opcode;
    $body = '' unless defined $body;

    my $sock = IO::Socket::INET->new(
        PeerAddr => $host,
        PeerPort => $port,
        Proto    => 'tcp',
        Timeout  => 5,
    ) or die "connect to $host:$port failed: $!\n";
    binmode($sock);

    write_frame( $sock, $opcode, $body );
    my $frame = $queue_reply ? read_queue_reply($sock) : read_reply($sock);
    $sock->close;

    # One line, tab-separated, so the calling shell can split it without
    # worrying about embedded newlines (compact JSON never has any).
    print $frame->{opcode} . "\t" . $frame->{body} . "\n";
}

main() unless caller;

1;
