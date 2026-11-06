length == 1 and
.[0].deletionTimestamp == null and
.[0].phase == "Running" and
([.[0].specContainers[]] | length) == 1 and
.[0].specContainers[0].image == $image and
([.[0].containers[]] | length) == 1 and
all(.[0].containers[]; .ready == true and (.imageID | endswith($digest)))
