#!/usr/bin/env perl
# Pure unit tests for scripts/lib/fcast-client.pl's framing and
# queue-reply-skip logic: no daemon, no network -- frames are built/read
# against in-memory scalar filehandles. Run with: perl scripts/tests/test_fcast_client.pl
use strict;
use warnings;
use Test::More;
use FindBin;
require "$FindBin::Bin/../lib/fcast-client.pl";

# A filehandle over an in-memory byte buffer, for feeding read_frame/
# read_queue_reply canned bytes without a real socket.
sub mem_fh {
    my ($bytes) = @_;
    open( my $fh, '<', \$bytes ) or die "open scalar fh failed: $!";
    binmode($fh);
    return $fh;
}

subtest 'encode_frame matches the documented wire format exactly' => sub {
    # README's "Manual protocol test" Ping example: length=1, opcode=12.
    is( encode_frame( 12, '' ), "\x01\x00\x00\x00\x0c", 'body-less Ping frame' );

    # README's SetVolume example.
    my $body = '{"volume":0.5}';
    is(
        encode_frame( 8, $body ),
        pack( 'V', length($body) + 1 ) . chr(8) . $body,
        'SetVolume frame with a JSON body'
    );
};

subtest 'encode_frame refuses an oversized frame' => sub {
    my $too_big = 'x' x ( $main::MAX_PACKET_SIZE );
    eval { encode_frame( 1, $too_big ) };
    like( $@, qr/frame too large/, 'dies rather than emitting a frame over the 32 KiB cap' );
};

subtest 'read_frame round-trips encode_frame, with and without a body' => sub {
    my $fh = mem_fh( encode_frame( 13, '' ) );
    my $frame = read_frame($fh);
    is( $frame->{opcode}, 13, 'opcode decoded' );
    is( $frame->{body},   '', 'empty body decoded' );

    $fh = mem_fh( encode_frame( 6, '{"generationTime":1,"state":1}' ) );
    $frame = read_frame($fh);
    is( $frame->{opcode}, 6, 'opcode decoded' );
    is( $frame->{body}, '{"generationTime":1,"state":1}', 'JSON body decoded verbatim' );
};

subtest 'read_frame returns undef on a clean EOF before any frame starts' => sub {
    my $fh = mem_fh('');
    is( read_frame($fh), undef, 'no frame, no error' );
};

subtest 'read_frame dies on a frame truncated mid-payload' => sub {
    my $full = encode_frame( 15, '{"items":[]}' );
    my $fh = mem_fh( substr( $full, 0, length($full) - 3 ) );
    eval { read_frame($fh) };
    like( $@, qr/unexpected end of stream/, 'a partial frame is a protocol error, not a clean EOF' );
};

subtest 'read_frame dies on an invalid length prefix' => sub {
    my $fh = mem_fh( pack( 'V', 0 ) );
    eval { read_frame($fh) };
    like( $@, qr/invalid FCast frame length/, 'zero length is rejected' );

    $fh = mem_fh( pack( 'V', $main::MAX_PACKET_SIZE + 1 ) );
    eval { read_frame($fh) };
    like( $@, qr/invalid FCast frame length/, 'a length over the 32 KiB cap is rejected' );
};

subtest 'read_queue_reply skips an interleaved PlaybackUpdate' => sub {
    # Mirrors ClearQueue's real behavior (and FCastClient.kt's
    # readQueueStateReply doc comment): a PlaybackUpdate publish can land on
    # this connection before the QueueState reply to the same command.
    my $bytes = encode_frame( $main::OPCODE_PLAYBACK_UPDATE, '{"generationTime":1,"state":0}' )
        . encode_frame( $main::OPCODE_QUEUE_STATE, '{"generationTime":2,"items":[],"currentIndex":null}' );
    my $fh = mem_fh($bytes);
    my $frame = read_queue_reply($fh);
    is( $frame->{opcode}, $main::OPCODE_QUEUE_STATE, 'the PlaybackUpdate was skipped, not returned' );
    is(
        $frame->{body},
        '{"generationTime":2,"items":[],"currentIndex":null}',
        'the QueueState body is the one actually returned'
    );
};

subtest 'read_queue_reply returns immediately when there is no interleaved frame' => sub {
    my $fh = mem_fh( encode_frame( $main::OPCODE_QUEUE_STATE, '{"items":[]}' ) );
    my $frame = read_queue_reply($fh);
    is( $frame->{opcode}, $main::OPCODE_QUEUE_STATE, 'a lone QueueState reply still works' );
};

subtest 'read_queue_reply dies if the connection closes before QueueState arrives' => sub {
    my $fh = mem_fh( encode_frame( $main::OPCODE_PLAYBACK_UPDATE, '{"state":0}' ) );
    eval { read_queue_reply($fh) };
    like( $@, qr/connection closed/, 'EOF after only a PlaybackUpdate is an error, not silently ignored' );
};

subtest 'read_queue_reply dies on an unexpected non-QueueState opcode' => sub {
    my $fh = mem_fh( encode_frame( 9, '{"message":"boom"}' ) );    # PlaybackError
    eval { read_queue_reply($fh) };
    like( $@, qr/expected a QueueState reply/, 'a reply that is neither skip nor stop opcode is an error' );
};

done_testing();
