# Java agent that tracks threads of java application

At the moment it cannot be built as it is required patched `rust-jvmti` library.

*Note*: it is not production ready, just a proof of concept

## Arguments

Socket path is a path to unix socket where threads information will be streamed.

## How to run

Run an application:

``` sh
java -agentpath:libjava_threads_tracker.so="/tmp/my-service-threads.sock" Main
```

And watch the application's threads:

``` sh
socat - UNIX-CONNECT:/tmp/my-service-threads.sock
```

You should see something like:

```
+292923: main
+292933: Signal Dispatcher
+292936: JVMCI-native CompilerThread0
+292931: Reference Handler
+292942: Notification Thread
+292932: Finalizer
+292948: thread_4
+292939: Common-Cleaner
+292954: thread_6
-292948: thread_4
+292955: thread_7
-292952: thread_5
+292956: thread_8
-292954: thread_6
```

## Why is it needed?

It is possible to apply some limits for threads according to their names. You can set `IO` priority or throttle `CPU` for specific threads.
For example, you can limit `IO` bandwidth for Elasticsearch's `force_merge` thread which doesn't have a corresponding setting.
