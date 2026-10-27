# Required IAM scope of the aws role

Moved from design.md section 12.4. Section numbers refer to design.md.

**Required IAM scope of the aws role** (a precondition of step 32): (a) assumed with a short session and an external id, the trust
policy naming only the scheduler's identity; (b) `ec2:RunInstances` only with `ec2:LaunchTemplate` equal to the configured template and
the configured subnets, and `iam:PassRole` only for the template's instance profile; (c) `TerminateInstances` only with
`ec2:ResourceTag/jobscheduler.dev/scheduler` equal to this scheduler's instance id; (d) `CreateTags` only with
`ec2:CreateAction=RunInstances`, so the role cannot re-tag a foreign instance to make it terminable; (e) Describe calls read-only. A
test checks the policy document the provider documents against (b) to (d).
