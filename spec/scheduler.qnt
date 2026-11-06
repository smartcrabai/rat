module scheduler {
  type JobId = str
  type Occurrence = str
  type JobState = str
  type RunState = str

  // Finite abstraction: one one-off job, one recurring job, and three
  // recurring occurrences. Integer time is restricted to ticks 0 through 3.
  pure val JOBS = Set("once", "repeat")
  pure val OCCURRENCES = Set("once0", "repeat0", "repeat1", "repeat2")

  var jobState: JobId -> JobState
  var nextAt: JobId -> Occurrence
  var now: int
  var runStatus: Occurrence -> RunState
  var reservationCount: Occurrence -> int
  var startedAfterDelete: JobId -> bool

  pure def owner(occurrence: Occurrence): JobId =
    if (occurrence == "once0") "once" else "repeat"

  pure def scheduledTime(occurrence: Occurrence): int =
    if (occurrence == "once0" or occurrence == "repeat0") 1
    else if (occurrence == "repeat1") 2
    else 3

  pure def firstFutureRecurring(time: int): Occurrence =
    if (time < 1) "repeat0"
    else if (time < 2) "repeat1"
    else if (time < 3) "repeat2"
    else "done"

  def inFlight(occurrence: Occurrence): bool =
    runStatus.get(occurrence) == "Starting" or runStatus.get(occurrence) == "Running"

  def hasInFlight(job: JobId): bool =
    OCCURRENCES.exists(occurrence => owner(occurrence) == job and inFlight(occurrence))

  action init = all {
    jobState' = JOBS.mapBy(_ => "Active"),
    nextAt' = JOBS.mapBy(job => if (job == "once") "once0" else "repeat0"),
    now' = 0,
    runStatus' = OCCURRENCES.mapBy(_ => "Absent"),
    reservationCount' = OCCURRENCES.mapBy(_ => 0),
    startedAfterDelete' = JOBS.mapBy(_ => false),
  }

  action advanceTime = all {
    now < 3,
    now' = now + 1,
    jobState' = jobState,
    nextAt' = nextAt,
    runStatus' = runStatus,
    reservationCount' = reservationCount,
    startedAfterDelete' = startedAfterDelete,
  }

  // Reservation advances a recurring pointer immediately. If that job is
  // already in flight, this occurrence is terminally Skipped, never queued.
  action reserve(job: JobId, occurrence: Occurrence): bool = all {
    jobState.get(job) == "Active",
    nextAt.get(job) == occurrence,
    scheduledTime(occurrence) <= now,
    runStatus.get(occurrence) == "Absent",
    reservationCount.get(occurrence) == 0,
    if (hasInFlight(job)) {
      all {
        job == "repeat",
        runStatus' = runStatus.set(occurrence, "Skipped"),
        nextAt' = nextAt.set(job, firstFutureRecurring(now)),
      }
    } else {
      all {
        runStatus' = runStatus.set(occurrence, "Starting"),
        nextAt' = nextAt.set(
          job,
          if (job == "once") "done" else firstFutureRecurring(now)
        ),
      }
    },
    jobState' = jobState,
    now' = now,
    reservationCount' = reservationCount.set(
      occurrence,
      reservationCount.get(occurrence) + 1
    ),
    startedAfterDelete' = startedAfterDelete,
  }

  action markRunning(occurrence: Occurrence): bool = all {
    runStatus.get(occurrence) == "Starting",
    jobState.get(owner(occurrence)) != "Deleted",
    runStatus' = runStatus.set(occurrence, "Running"),
    startedAfterDelete' = startedAfterDelete.set(
      owner(occurrence),
      startedAfterDelete.get(owner(occurrence)) or jobState.get(owner(occurrence)) == "Deleted"
    ),
    jobState' = jobState,
    nextAt' = nextAt,
    now' = now,
    reservationCount' = reservationCount,
  }

  action finish(occurrence: Occurrence, failed: bool): bool = all {
    runStatus.get(occurrence) == "Running",
    runStatus' = runStatus.set(occurrence, if (failed) "Failed" else "Succeeded"),
    jobState' = if (owner(occurrence) == "once" and jobState.get("once") != "Deleted") {
      jobState.set("once", "Completed")
    } else {
      jobState
    },
    nextAt' = nextAt,
    now' = now,
    reservationCount' = reservationCount,
    startedAfterDelete' = startedAfterDelete,
  }

  action pause(job: JobId): bool = all {
    jobState.get(job) == "Active",
    jobState' = jobState.set(job, "Paused"),
    nextAt' = nextAt,
    now' = now,
    runStatus' = runStatus,
    reservationCount' = reservationCount,
    startedAfterDelete' = startedAfterDelete,
  }

  action resume(job: JobId, confirmOverdueOneOff: bool): bool = all {
    jobState.get(job) == "Paused",
    if (job == "once") {
      not(scheduledTime("once0") <= now) or confirmOverdueOneOff
    } else {
      true
    },
    jobState' = jobState.set(job, "Active"),
    nextAt' = if (job == "repeat") {
      nextAt.set(job, firstFutureRecurring(now))
    } else {
      nextAt
    },
    now' = now,
    runStatus' = runStatus,
    reservationCount' = reservationCount,
    startedAfterDelete' = startedAfterDelete,
  }

  action recover = all {
    runStatus' = OCCURRENCES.mapBy(
      occurrence => if (inFlight(occurrence)) "Uncertain" else runStatus.get(occurrence)
    ),
    jobState' = JOBS.mapBy(
      job => if (hasInFlight(job) and jobState.get(job) != "Deleted") "NeedsReview" else jobState.get(job)
    ),
    nextAt' = JOBS.mapBy(
      job => if (job == "repeat") firstFutureRecurring(now) else nextAt.get(job)
    ),
    now' = now,
    reservationCount' = reservationCount,
    startedAfterDelete' = startedAfterDelete,
  }

  action resolve(job: JobId): bool = all {
    jobState.get(job) == "NeedsReview",
    jobState' = jobState.set(
      job,
      if (job == "once") "Completed" else "Active"
    ),
    nextAt' = if (job == "repeat") {
      nextAt.set(job, firstFutureRecurring(now))
    } else {
      nextAt
    },
    now' = now,
    runStatus' = runStatus,
    reservationCount' = reservationCount,
    startedAfterDelete' = startedAfterDelete,
  }

  action delete(job: JobId): bool = all {
    jobState' = jobState.set(job, "Deleted"),
    nextAt' = nextAt,
    now' = now,
    runStatus' = runStatus,
    reservationCount' = reservationCount,
    startedAfterDelete' = startedAfterDelete,
  }

  action stutter = all {
    jobState' = jobState,
    nextAt' = nextAt,
    now' = now,
    runStatus' = runStatus,
    reservationCount' = reservationCount,
    startedAfterDelete' = startedAfterDelete,
  }

  val noDuplicateOccurrence =
    OCCURRENCES.forall(occurrence => reservationCount.get(occurrence) <= 1)

  val noSameJobOverlap = OCCURRENCES.forall(left => OCCURRENCES.forall(right =>
    left == right or owner(left) != owner(right) or not(inFlight(left) and inFlight(right))
  ))

  val needsReviewCannotDispatch = JOBS.forall(job =>
    jobState.get(job) != "NeedsReview" or
      OCCURRENCES.forall(occurrence => owner(occurrence) != job or not(inFlight(occurrence)))
  )

  val failedOccurrenceNotReplayed = OCCURRENCES.forall(occurrence =>
    runStatus.get(occurrence) != "Failed" or
      (reservationCount.get(occurrence) == 1 and nextAt.get(owner(occurrence)) != occurrence)
  )

  val uncertainOccurrenceNotReplayed = OCCURRENCES.forall(occurrence =>
    runStatus.get(occurrence) != "Uncertain" or
      (reservationCount.get(occurrence) == 1 and nextAt.get(owner(occurrence)) != occurrence)
  )

  // A run already Running may finish after deletion; no new run may start then.
  val deletedJobNeverStartsAgain = JOBS.forall(job => not(startedAfterDelete.get(job)))

  val schedulerSafety = all {
    noDuplicateOccurrence,
    noSameJobOverlap,
    needsReviewCannotDispatch,
    failedOccurrenceNotReplayed,
    uncertainOccurrenceNotReplayed,
    deletedJobNeverStartsAgain,
  }

  action step = any {
    advanceTime,
    reserve("once", "once0"),
    reserve("repeat", "repeat0"),
    reserve("repeat", "repeat1"),
    reserve("repeat", "repeat2"),
    markRunning("once0"),
    markRunning("repeat0"),
    markRunning("repeat1"),
    markRunning("repeat2"),
    finish("once0", false),
    finish("once0", true),
    finish("repeat0", false),
    finish("repeat0", true),
    finish("repeat1", false),
    finish("repeat1", true),
    finish("repeat2", false),
    finish("repeat2", true),
    pause("once"),
    pause("repeat"),
    resume("once", false),
    resume("once", true),
    resume("repeat", false),
    recover,
    resolve("once"),
    resolve("repeat"),
    delete("once"),
    delete("repeat"),
    stutter,
  }

  run reservationCreatesOneStartingRunTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(all {
        assert(runStatus.get("once0") == "Starting"),
        assert(reservationCount.get("once0") == 1),
        assert(nextAt.get("once") == "done"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
  }

  run duplicateOccurrenceReservationIsRejectedTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(all {
        assert(reservationCount.get("once0") == 1),
        assert(runStatus.get("once0") == "Starting"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("once", "once0").fail())
  }

  run overlappingRecurringOccurrenceIsSkippedNotQueuedTest = {
    init.then(advanceTime)
      .then(reserve("repeat", "repeat0"))
      .then(markRunning("repeat0"))
      .then(advanceTime)
      .then(reserve("repeat", "repeat1"))
      .then(all {
        assert(runStatus.get("repeat0") == "Running"),
        assert(runStatus.get("repeat1") == "Skipped"),
        assert(reservationCount.get("repeat1") == 1),
        assert(nextAt.get("repeat") == "repeat2"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(finish("repeat0", false))
      .then(all {
        assert(runStatus.get("repeat1") == "Skipped"),
        assert(not(inFlight("repeat1"))),
        assert(nextAt.get("repeat") == "repeat2"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
  }

  run distinctJobsCanRunConcurrentlyTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(reserve("repeat", "repeat0"))
      .then(all {
        assert(inFlight("once0")),
        assert(inFlight("repeat0")),
        assert(noSameJobOverlap),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
  }

  run failedOneOffBecomesTerminalTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(markRunning("once0"))
      .then(finish("once0", true))
      .then(all {
        assert(runStatus.get("once0") == "Failed"),
        assert(jobState.get("once") == "Completed"),
        assert(nextAt.get("once") == "done"),
        assert(reservationCount.get("once0") == 1),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("once", "once0").fail())
  }

  run failedRecurringOccurrenceDoesNotStopScheduleTest = {
    init.then(advanceTime)
      .then(reserve("repeat", "repeat0"))
      .then(markRunning("repeat0"))
      .then(finish("repeat0", true))
      .then(all {
        assert(runStatus.get("repeat0") == "Failed"),
        assert(jobState.get("repeat") == "Active"),
        assert(nextAt.get("repeat") == "repeat1"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(advanceTime)
      .then(reserve("repeat", "repeat1"))
      .then(all {
        assert(runStatus.get("repeat0") == "Failed"),
        assert(runStatus.get("repeat1") == "Starting"),
        assert(jobState.get("repeat") == "Active"),
        assert(nextAt.get("repeat") == "repeat2"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
  }

  run pausedOneOffNeedsOverdueConfirmationTest = {
    init.then(pause("once"))
      .then(advanceTime)
      .then(resume("once", true))
      .then(reserve("once", "once0"))
      .then(all {
        assert(jobState.get("once") == "Active"),
        assert(runStatus.get("once0") == "Starting"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
  }

  run pausedOneOffCannotBeReservedTest = {
    init.then(pause("once"))
      .then(advanceTime)
      .then(all {
        assert(jobState.get("once") == "Paused"),
        assert(runStatus.get("once0") == "Absent"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("once", "once0").fail())
  }

  run overdueOneOffCannotResumeWithoutConfirmationTest = {
    init.then(pause("once"))
      .then(advanceTime)
      .then(all {
        assert(now == 1),
        assert(jobState.get("once") == "Paused"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(resume("once", false).fail())
  }
  run resumedRecurringJobSkipsToStrictlyFutureOccurrenceTest = {
    init.then(pause("repeat"))
      .then(advanceTime)
      .then(advanceTime)
      .then(resume("repeat", false))
      .then(advanceTime)
      .then(reserve("repeat", "repeat2"))
      .then(all {
        assert(jobState.get("repeat") == "Active"),
        assert(now == 3),
        assert(runStatus.get("repeat0") == "Absent"),
        assert(runStatus.get("repeat1") == "Absent"),
        assert(runStatus.get("repeat2") == "Starting"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
  }

  run resumedRecurringJobRejectsPastOccurrenceZeroTest = {
    init.then(pause("repeat"))
      .then(advanceTime)
      .then(advanceTime)
      .then(resume("repeat", false))
      .then(all {
        assert(now == 2),
        assert(nextAt.get("repeat") == "repeat2"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("repeat", "repeat0").fail())
  }

  run resumedRecurringJobRejectsPastOccurrenceOneTest = {
    init.then(pause("repeat"))
      .then(advanceTime)
      .then(advanceTime)
      .then(resume("repeat", false))
      .then(all {
        assert(now == 2),
        assert(nextAt.get("repeat") == "repeat2"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("repeat", "repeat1").fail())
  }

  run uncertainRunsRequireReviewAndAreNeverReplayedTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(reserve("repeat", "repeat0"))
      .then(markRunning("once0"))
      .then(markRunning("repeat0"))
      .then(recover)
      .then(advanceTime)
      .then(resolve("once"))
      .then(resolve("repeat"))
      .then(all {
        assert(jobState.get("once") == "Completed"),
        assert(jobState.get("repeat") == "Active"),
        assert(runStatus.get("once0") == "Uncertain"),
        assert(runStatus.get("repeat0") == "Uncertain"),
        assert(reservationCount.get("once0") == 1),
        assert(reservationCount.get("repeat0") == 1),
        assert(nextAt.get("repeat") == "repeat2"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
  }

  run needsReviewRecurringCannotDispatchTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(reserve("repeat", "repeat0"))
      .then(markRunning("once0"))
      .then(markRunning("repeat0"))
      .then(recover)
      .then(advanceTime)
      .then(all {
        assert(jobState.get("repeat") == "NeedsReview"),
        assert(runStatus.get("repeat0") == "Uncertain"),
        assert(nextAt.get("repeat") == "repeat1"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("repeat", "repeat1").fail())
  }

  run needsReviewOneOffCannotDispatchTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(markRunning("once0"))
      .then(recover)
      .then(all {
        assert(jobState.get("once") == "NeedsReview"),
        assert(runStatus.get("once0") == "Uncertain"),
        assert(reservationCount.get("once0") == 1),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("once", "once0").fail())
  }

  run resolvedUncertainOneOffCannotBeReplayedTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(reserve("repeat", "repeat0"))
      .then(markRunning("once0"))
      .then(markRunning("repeat0"))
      .then(recover)
      .then(advanceTime)
      .then(resolve("once"))
      .then(resolve("repeat"))
      .then(all {
        assert(jobState.get("once") == "Completed"),
        assert(runStatus.get("once0") == "Uncertain"),
        assert(reservationCount.get("once0") == 1),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("once", "once0").fail())
  }

  run resolvedUncertainRecurringOccurrenceCannotBeReplayedTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(reserve("repeat", "repeat0"))
      .then(markRunning("once0"))
      .then(markRunning("repeat0"))
      .then(recover)
      .then(advanceTime)
      .then(resolve("once"))
      .then(resolve("repeat"))
      .then(all {
        assert(jobState.get("repeat") == "Active"),
        assert(runStatus.get("repeat0") == "Uncertain"),
        assert(reservationCount.get("repeat0") == 1),
        assert(nextAt.get("repeat") == "repeat2"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("repeat", "repeat0").fail())
  }

  run deletingJobDoesNotStopItsRunningCommandTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(markRunning("once0"))
      .then(delete("once"))
      .then(all {
        assert(jobState.get("once") == "Deleted"),
        assert(runStatus.get("once0") == "Running"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(finish("once0", false))
      .then(all {
        assert(jobState.get("once") == "Deleted"),
        assert(runStatus.get("once0") == "Succeeded"),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(reserve("once", "once0").fail())
  }

  run deletingJobWhileStartingCannotBecomeRunningTest = {
    init.then(advanceTime)
      .then(reserve("once", "once0"))
      .then(delete("once"))
      .then(all {
        assert(jobState.get("once") == "Deleted"),
        assert(runStatus.get("once0") == "Starting"),
        assert(not(startedAfterDelete.get("once"))),
        jobState' = jobState,
        nextAt' = nextAt,
        now' = now,
        runStatus' = runStatus,
        reservationCount' = reservationCount,
        startedAfterDelete' = startedAfterDelete,
      })
      .then(markRunning("once0").fail())
  }

}
