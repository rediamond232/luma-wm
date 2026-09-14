public final class LumaAttachFixture {
    private static native void run(int seconds);

    public static void main(String[] arguments) throws Exception {
        if (arguments.length != 2) {
            throw new IllegalArgumentException("expected native library path and duration");
        }
        System.load(arguments[0]);
        System.out.println("READY");
        System.out.flush();
        Thread.sleep(1000);
        run(Integer.parseInt(arguments[1]));
        System.out.println("JAVA_DONE");
        System.out.flush();
        System.exit(0);
    }
}
