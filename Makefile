##
# Project Title
#
# @file
# @version 0.1

run:
	java -agentpath:$$(pwd)/target/debug/libjava_threads_tracker.so='/tmp/java-threads.sock' Main

# end
