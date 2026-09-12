#!/usr/bin/env perl
use strict;
use warnings;
use File::Spec;
use Time::HiRes qw(time);

my ($label, $binary, $samples) = @ARGV;
die "Usage: measure-startup.pl LABEL BINARY SAMPLES\n" unless defined $samples && $samples =~ /^\d+$/ && $samples > 0;
die "Not executable: $binary\n" unless -x $binary;
open my $report, '>&', \*STDOUT or die "Cannot preserve stdout: $!\n";
open STDOUT, '>', File::Spec->devnull() or die "Cannot open null device: $!\n";
open STDERR, '>', File::Spec->devnull() or die "Cannot open null device: $!\n";

my @milliseconds;
for (1 .. $samples) {
    my $start = time();
    system {$binary} $binary, '--version';
    die "$binary --version failed\n" unless $? == 0;
    push @milliseconds, (time() - $start) * 1000;
}
@milliseconds = sort { $a <=> $b } @milliseconds;
my $p50 = $milliseconds[int(($samples - 1) * 0.50)];
my $p95 = $milliseconds[int(($samples - 1) * 0.95)];
printf {$report} "%s\t%d\t%.3f\t%.3f\t%.3f\n", $label, $samples, $p50, $p95, $milliseconds[-1];
