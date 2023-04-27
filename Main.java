import com.sun.nio.file.ExtendedOpenOption;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.OpenOption;
import java.nio.file.Paths;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.concurrent.CountDownLatch;

public class Main {
    public static void main(String[] args) throws InterruptedException {
        final var num_threads = 10;
        for (var i = 1; i <= num_threads; i++) {
            startThread(String.format("thread_%d", i), 11000);
            Thread.sleep(5000);
        }
    }

    private static Thread startThread(String name, long period) {
        final var thread = new Thread(() -> {
            try {
                System.out.printf("Thread %s started\n", name);
                Thread.sleep(period);
            } catch (InterruptedException e) {
                System.out.printf("Thread %s was interrupted\n", name);
            }
        }, name);
        thread.start();
        return thread;
    }
}
