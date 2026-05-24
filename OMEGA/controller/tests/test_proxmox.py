from controller.proxmox import ProxmoxClient


class TestParseOmegaMetadata:
    def test_derives_vcpus_from_sockets_and_cores(self):
        config = {
            "sockets": 2,
            "cores": 4,
        }
        data = ProxmoxClient.parse_omega_metadata(config)
        assert data["max_vcpus"] == 8
        assert data["min_vcpus"] == 4

    def test_reads_description_metadata(self):
        config = {
            "description": "\n".join([
                "service web",
                "omega.gpu_vram_mib=2048",
                "omega.min_vcpus=2",
                "omega.max_vcpus=8",
            ])
        }
        data = ProxmoxClient.parse_omega_metadata(config)
        assert data["gpu_vram_mib"] == 2048
        assert data["min_vcpus"] == 2
        assert data["max_vcpus"] == 8

    def test_reads_tag_metadata(self):
        config = {
            "tags": "prod;omega-gpu-4096;omega-min-vcpus-1;omega-max-vcpus-4"
        }
        data = ProxmoxClient.parse_omega_metadata(config)
        assert data["gpu_vram_mib"] == 4096
        assert data["min_vcpus"] == 1
        assert data["max_vcpus"] == 4

    def test_defaults_min_to_half_of_explicit_max(self):
        config = {
            "description": "omega.max_vcpus=8",
            "sockets": 1,
            "cores": 2,
        }
        data = ProxmoxClient.parse_omega_metadata(config)
        assert data["min_vcpus"] == 4
        assert data["max_vcpus"] == 8

    def test_clamps_min_to_max(self):
        config = {
            "description": "omega.min_vcpus=8\nomega.max_vcpus=4"
        }
        data = ProxmoxClient.parse_omega_metadata(config)
        assert data["min_vcpus"] == 4
        assert data["max_vcpus"] == 4

    def test_missing_metadata_defaults_to_topology(self):
        data = ProxmoxClient.parse_omega_metadata({})
        assert data["gpu_vram_mib"] == 0
        assert data["min_vcpus"] == 1
        assert data["max_vcpus"] == 1
